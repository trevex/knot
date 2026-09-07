//! The per-user notification inbox.
//!
//! GET  /api/notifications?filter=unread|all&limit=&cursor=  → { items, next_cursor }
//! GET  /api/notifications/unread_count                      → { count, capped }
//! POST /api/notifications/read  { ids: [] } | { all: true } → 204
//!
//! Rows are addressed to exactly one recipient, so ownership needs no join.
//! Document *access* is a separate question: a notification can outlive the
//! grant that made it visible, so rows carrying a `doc_id` are re-checked
//! against `effective_role` and filtered, never 403'd.

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;
const UNREAD_CAP: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/unread_count", get(unread_count))
        .route("/api/notifications/read", post(mark_read))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<i64>,
}

#[derive(Serialize)]
struct NotificationRow {
    id: i64,
    kind: String,
    doc_id: Option<String>,
    doc_title: Option<String>,
    target_kind: String,
    target_id: String,
    actor_display_name: Option<String>,
    data: serde_json::Value,
    created_at: String,
    read: bool,
}

#[derive(Serialize)]
struct ListResponse {
    items: Vec<NotificationRow>,
    next_cursor: Option<i64>,
}

#[derive(Serialize)]
struct CountResponse {
    count: i64,
    capped: bool,
}

#[derive(Deserialize)]
struct ReadBody {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    all: bool,
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(notifications), Some(acl)) = (state.notifications.clone(), state.acl.clone()) else {
        return internal();
    };
    let unread_only = q.filter.as_deref() == Some("unread");
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows = match notifications
        .list(ctx.user_id, unread_only, limit, q.cursor)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error=?e, "notifications list");
            return internal();
        }
    };
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };

    // Filter, don't fail: access can be revoked after the row is written.
    // Rows are independent (each keyed by its own doc_id), so the checks run
    // concurrently rather than one `.await` per row in sequence — a
    // cache-cold page of up to `MAX_LIMIT` rows would otherwise serialize
    // that many round trips through `acl::resolve` behind one response.
    let checks = futures::future::join_all(rows.iter().map(|r| {
        let acl = &acl;
        async move {
            match r.doc_id {
                Some(doc_id) => Some(
                    acl.effective_role(ctx.workspace_id, doc_id, ctx.user_id)
                        .await,
                ),
                None => None,
            }
        }
    }))
    .await;

    let mut items = Vec::with_capacity(rows.len());
    for (r, check) in rows.into_iter().zip(checks) {
        match check {
            None => {}                  // no doc_id: nothing to re-check, keep.
            Some(Ok(Some(_))) => {}     // access confirmed, keep.
            Some(Ok(None)) => continue, // access revoked: drop the row silently.
            Some(Err(e)) => {
                // An ACL lookup failure is a database problem, not a stale
                // grant — fail the whole request rather than silently
                // dropping the row, which would look like data loss to the
                // user.
                tracing::error!(error=?e, "notifications acl check");
                return internal();
            }
        }
        items.push(NotificationRow {
            id: r.id,
            kind: r.kind,
            doc_id: r.doc_id.map(|d| d.to_string()),
            doc_title: r.doc_title,
            target_kind: r.target_kind,
            target_id: r.target_id,
            actor_display_name: r.actor_display_name,
            data: r.data,
            created_at: r.created_at.to_rfc3339(),
            read: r.read_at.is_some(),
        });
    }

    Json(ListResponse { items, next_cursor }).into_response()
}

// Unlike `list`, this does not re-check `effective_role` per row, so the
// count can in principle include rows for documents the caller can no
// longer read — the badge could read higher than what `list` returns.
// That's unreachable today: v0.1 is single-workspace-per-deployment, and
// workspace membership alone grants a role on every doc in it, so a
// request that reaches this handler can never hit the `effective_role ->
// None` branch. It becomes reachable once a deployment can hold multiple
// workspaces; the fix then is a `workspace_members` join inside the
// existing `LIMIT` subquery in the store, not per-row ACL calls here,
// which would reintroduce the sequential-scan risk the cap exists to
// avoid.
async fn unread_count(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(notifications) = state.notifications.clone() else {
        return internal();
    };
    match notifications.unread_count(ctx.user_id, UNREAD_CAP).await {
        Ok(n) => Json(CountResponse {
            count: n,
            capped: is_capped(n, UNREAD_CAP),
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications unread_count");
            internal()
        }
    }
}

/// Whether the badge should show the "+" suffix: `n` hit the `LIMIT` the
/// store's `unread_count` query imposes, so the true count might be
/// higher. Pulled out of the handler so the `true` branch has direct unit
/// coverage — exercising it through the HTTP stack would mean actually
/// creating `UNREAD_CAP` (100) rows for a user, which no existing test does.
fn is_capped(n: i64, cap: i64) -> bool {
    n >= cap
}

async fn mark_read(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(notifications) = state.notifications.clone() else {
        return internal();
    };
    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "bad_request", ""),
    };
    let body: ReadBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    let res = if body.all {
        notifications.mark_all_read(ctx.user_id).await
    } else {
        notifications.mark_read(ctx.user_id, &body.ids).await
    };
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications mark_read");
            internal()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_capped;

    #[test]
    fn not_capped_below_the_limit() {
        assert!(!is_capped(4, 5));
    }

    #[test]
    fn capped_at_and_above_the_limit() {
        assert!(is_capped(5, 5), "count == cap must already read as capped");
        assert!(is_capped(6, 5));
    }
}
