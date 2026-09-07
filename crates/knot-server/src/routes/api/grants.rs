//! Document grants API:
//! - GET    /api/docs/{id}/grants
//! - PUT    /api/docs/{id}/grants/{principal}   body: {role, inherit}
//! - DELETE /api/docs/{id}/grants/{principal}
//!
//! Mounted into the docs router so the shared `require_doc_role_mw` layer
//! covers these routes too.

use axum::{
    Json,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use knot_storage::WorkspaceRole;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

#[derive(Serialize)]
struct GrantResponse {
    principal: String,
    role: String,
    inherit: bool,
}

pub(super) async fn list_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    }
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants.list(doc_id).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|g| GrantResponse {
                    principal: g.principal,
                    role: g.role.as_str().into(),
                    inherit: g.inherit,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "grants list");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct PutGrantRequest {
    role: String,
    inherit: bool,
}

pub(super) async fn put_inline(
    State(state): State<AppState>,
    Path((doc_id, principal)): Path<(Uuid, String)>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<PutGrantRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(new_role) = WorkspaceRole::parse(&body.role) else {
        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "grant.invalid_role", "");
    };
    if !is_valid_principal(&principal) {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "grant.invalid_principal",
            "",
        );
    }
    // `group:` principals pass the format check but ACL resolution only honors
    // `user:` grants — a group grant would silently grant nothing. Reject at
    // creation rather than store a no-op. (delete still accepts them so any
    // legacy group grant can be removed.)
    if principal.starts_with("group:") {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "grant.group_unsupported",
            "group grants are not supported; grant individual users",
        );
    }
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants
        .put(
            ctx.workspace_id,
            doc_id,
            &principal,
            new_role,
            body.inherit,
            ctx.user_id,
        )
        .await
    {
        Ok(()) => {
            if let Some(rest) = principal.strip_prefix("user:")
                && let Ok(grantee) = Uuid::parse_str(rest)
            {
                emit_doc_shared(&state, doc_id, grantee, ctx.user_id).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error=?e, "grants put");
            internal()
        }
    }
}

pub(super) async fn delete_inline(
    State(state): State<AppState>,
    Path((doc_id, principal)): Path<(Uuid, String)>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants
        .delete(ctx.workspace_id, doc_id, &principal, ctx.user_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "grants delete");
            internal()
        }
    }
}

fn is_valid_principal(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix("user:") {
        return Uuid::parse_str(rest).is_ok();
    }
    if let Some(rest) = s.strip_prefix("group:") {
        return !rest.is_empty();
    }
    false
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, ()> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

/// Tell the grantee a document was shared with them. Best-effort: a failure
/// here must not fail the grant that already committed.
async fn emit_doc_shared(state: &AppState, doc_id: Uuid, grantee: Uuid, actor: Uuid) {
    let (Some(notifications), Some(docs)) = (state.notifications.clone(), state.docs.clone())
    else {
        return;
    };
    let Ok(Some(doc)) = docs.get(doc_id).await else {
        return;
    };
    let n = knot_storage::NewNotification {
        workspace_id: doc.workspace_id,
        user_id: grantee,
        actor_id: Some(actor),
        kind: knot_storage::NotificationKind::DocShared,
        doc_id: Some(doc_id),
        target_kind: "document".into(),
        target_id: doc_id.to_string(),
        dedupe_key: format!("share:{doc_id}:{grantee}"),
        data: serde_json::json!({ "doc_title": doc.title }),
    };
    if let Err(e) = notifications.emit(&n).await {
        tracing::warn!(error=?e, %doc_id, "emit doc_shared");
    }
}
