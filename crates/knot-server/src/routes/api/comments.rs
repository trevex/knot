//! Comment threads on documents.
//!
//! POST   /api/docs/{doc_id}/comments                  { body, position_y?, anchor_text? } → 201
//! POST   /api/docs/{doc_id}/comments/{thread_id}/replies { body } → 201
//! GET    /api/docs/{doc_id}/comments?include_resolved  → 200 [Comment]
//!
//! POST   /api/docs/{doc_id}/comments/{thread_id}/resolve   → 204
//! POST   /api/docs/{doc_id}/comments/{thread_id}/unresolve → 204
//!
//! POST   /api/docs/{doc_id}/comments/{comment_id}/reactions        { emoji } → 204
//! DELETE /api/docs/{doc_id}/comments/{comment_id}/reactions/{emoji}  → 204
//!
//! PATCH  /api/docs/{doc_id}/comments/{comment_id} { body } → 200 (author only)
//! DELETE /api/docs/{doc_id}/comments/{comment_id}          → 204 (author or workspace owner)

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, patch, post},
};
use knot_storage::{CommentStoreError, WorkspaceRole};
use regex::Regex;
use serde::Deserialize;
use uuid::Uuid;

/// Compiled once at first use; avoids re-compiling on every comment request.
static MENTION_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?:^|\s)@(\w+)").expect("valid mention regex"));

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

// ---------------------------------------------------------------------------
// Allowed emojis
// ---------------------------------------------------------------------------

const ALLOWED_EMOJIS: &[&str] = &["👍", "🎉", "❤️", "🚀", "👀", "🙏"];

fn emoji_allowed(e: &str) -> bool {
    ALLOWED_EMOJIS.contains(&e)
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateThreadBody {
    body: String,
    /// Base64-encoded Yjs RelativePosition bytes for the START of the range.
    #[serde(default)]
    position_y: Option<String>,
    /// Base64-encoded Yjs RelativePosition bytes for the END of the range.
    #[serde(default)]
    position_y_end: Option<String>,
    #[serde(default)]
    anchor_text: Option<String>,
    /// User ids the client's mention picker resolved. Preferred over the
    /// display-name regex, which cannot match a name containing a space.
    #[serde(default)]
    mentions: Vec<Uuid>,
}

#[derive(Deserialize)]
struct CreateReplyBody {
    body: String,
    /// User ids the client's mention picker resolved. Preferred over the
    /// display-name regex, which cannot match a name containing a space.
    #[serde(default)]
    mentions: Vec<Uuid>,
}

#[derive(Deserialize)]
struct EditBody {
    body: String,
}

#[derive(Deserialize)]
struct ReactionBody {
    emoji: String,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_resolved: bool,
}

// ---------------------------------------------------------------------------
// ACL helpers — read from extensions set by require_doc_role_mw
// ---------------------------------------------------------------------------

fn require_auth(req: &Request<Body>) -> Option<&AuthContext> {
    req.extensions().get::<AuthContext>()
}

fn require_editor(req: &Request<Body>) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    match req.extensions().get::<EffectiveDocRole>().copied() {
        None => Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", "")),
        Some(role) if role.0 == WorkspaceRole::Viewer => {
            Some(json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""))
        }
        Some(_) => None,
    }
}

fn require_viewer(req: &Request<Body>) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""));
    }
    None
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

// ---------------------------------------------------------------------------
// Mention extraction + broadcast
// ---------------------------------------------------------------------------

/// Extract @mention handles from comment body.
fn extract_mentions(body: &str) -> Vec<String> {
    MENTION_RE
        .captures_iter(body)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

/// Distinguishes a brand-new comment (thread open or reply) from an edit of
/// an existing one, for `emit_comment_notifications`. An edit is not a
/// reply — nobody said anything new to the thread — so it must not fan out
/// `reply` rows the way a new comment does; it can still add `mention`
/// rows, since the edited body may name someone for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentWrite {
    Created,
    Edited,
}

/// Write inbox rows for a comment write: `mention` for everyone named in the
/// body, plus (for a newly created comment only — see [`CommentWrite`])
/// `reply` for the thread's other participants. A user who is both gets the
/// mention only.
///
/// Runs after the comment has committed, so a crash in between loses the
/// notification. That is the same at-most-once behaviour the previous
/// `pg_notify` had, and a trait object cannot join the caller's transaction.
#[allow(clippy::too_many_arguments)] // cohesive set of comment-write context
async fn emit_comment_notifications(
    state: &AppState,
    doc_id: Uuid,
    thread_id: Uuid,
    comment_id: Uuid,
    author_id: Uuid,
    body: &str,
    write: CommentWrite,
    explicit: &[Uuid],
) {
    let (Some(notifications), Some(docs), Some(workspaces), Some(comments)) = (
        state.notifications.clone(),
        state.docs.clone(),
        state.workspaces.clone(),
        state.comments.clone(),
    ) else {
        return;
    };
    let Ok(Some(doc)) = docs.get(doc_id).await else {
        return;
    };

    // Explicit ids from the picker win; the regex is the fallback for
    // clients that don't send them (and every comment written before this
    // shipped). Either way, membership decides — an id for a non-member is
    // dropped rather than trusted.
    let members = match workspaces.list_members(doc.workspace_id).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let mentioned: Vec<Uuid> = if explicit.is_empty() {
        let handles = extract_mentions(body);
        members
            .iter()
            .filter(|m| handles.contains(&m.display_name.to_lowercase()))
            .map(|m| m.user_id)
            .collect()
    } else {
        members
            .iter()
            .filter(|m| explicit.contains(&m.user_id))
            .map(|m| m.user_id)
            .collect()
    };

    let excerpt: String = body.chars().take(140).collect();
    let base_data = serde_json::json!({
        "excerpt": excerpt,
        "doc_title": doc.title,
        "thread_id": thread_id.to_string(),
    });

    let mut batch: Vec<knot_storage::NewNotification> = mentioned
        .iter()
        .map(|&uid| knot_storage::NewNotification {
            workspace_id: doc.workspace_id,
            user_id: uid,
            actor_id: Some(author_id),
            kind: knot_storage::NotificationKind::Mention,
            doc_id: Some(doc_id),
            target_kind: "comment".into(),
            target_id: comment_id.to_string(),
            dedupe_key: format!("mention:{comment_id}"),
            data: base_data.clone(),
        })
        .collect();

    // Thread participants, minus the author and minus anyone already
    // receiving a mention for this comment. Only for a newly created
    // comment: editing an existing one is not a reply, and re-running this
    // fan-out on every edit would falsely tell participants someone just
    // replied.
    if write == CommentWrite::Created
        && let Ok(thread) = comments.list(doc_id, true).await
    {
        let mut seen: Vec<Uuid> = mentioned.clone();
        seen.push(author_id);
        for c in thread.into_iter().filter(|c| c.thread_id == thread_id) {
            if seen.contains(&c.author_id) {
                continue;
            }
            seen.push(c.author_id);
            batch.push(knot_storage::NewNotification {
                workspace_id: doc.workspace_id,
                user_id: c.author_id,
                actor_id: Some(author_id),
                kind: knot_storage::NotificationKind::Reply,
                doc_id: Some(doc_id),
                target_kind: "comment".into(),
                target_id: comment_id.to_string(),
                dedupe_key: format!("reply:{comment_id}"),
                data: base_data.clone(),
            });
        }
    }

    if batch.is_empty() {
        return;
    }
    if let Err(e) = notifications.emit_many(&batch).await {
        tracing::warn!(error=?e, %comment_id, "emit comment notifications");
    }
}

/// Fire-and-forget: tell any active room for `doc_id` that its comments changed,
/// so connected clients refetch. Reads `state.pool` directly, same as the
/// notification path reads its individual stores.
fn notify_comment_change(state: &AppState, doc_id: Uuid) {
    let Some(pool) = state.pool.as_ref().cloned() else {
        return;
    };
    let payload = serde_json::json!({ "doc_id": doc_id.to_string() }).to_string();
    tokio::spawn(async move {
        let _ = sqlx::query("SELECT pg_notify('doc_comments', $1)")
            .bind(&payload)
            .execute(&pool)
            .await;
    });
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// POST /api/docs/{doc_id}/comments
async fn create_thread(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: CreateThreadBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "comment.body_too_large",
            "body must be ≤ 4096 chars",
        );
    }

    // Decode position_y / position_y_end from base64 if present.
    let position_y: Option<Vec<u8>> = match body_req.position_y {
        None => None,
        Some(ref s) => {
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s) {
                Ok(b) => Some(b),
                Err(_) => {
                    return json_err(StatusCode::BAD_REQUEST, "comment.invalid_position_y", "");
                }
            }
        }
    };
    let position_y_end: Option<Vec<u8>> = match body_req.position_y_end {
        None => None,
        Some(ref s) => {
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s) {
                Ok(b) => Some(b),
                Err(_) => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "comment.invalid_position_y_end",
                        "",
                    );
                }
            }
        }
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    let explicit = body_req.mentions.clone();
    match comments
        .create_thread(
            doc_id,
            ctx.user_id,
            &body_req.body,
            position_y,
            position_y_end,
            body_req.anchor_text,
        )
        .await
    {
        Ok(c) => {
            let comment_id = c.id;
            let c_thread_id = c.thread_id;
            let body_text = c.body.clone();
            let response = (StatusCode::CREATED, Json(c)).into_response();
            emit_comment_notifications(
                &state,
                doc_id,
                c_thread_id,
                comment_id,
                ctx.user_id,
                &body_text,
                CommentWrite::Created,
                &explicit,
            )
            .await;
            notify_comment_change(&state, doc_id);
            response
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "create_thread");
            internal()
        }
    }
}

/// POST /api/docs/{doc_id}/comments/{thread_id}/replies
async fn create_reply(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: CreateReplyBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "");
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    let explicit = body_req.mentions.clone();
    match comments
        .create_reply(doc_id, thread_id, ctx.user_id, &body_req.body)
        .await
    {
        Ok(c) => {
            let comment_id = c.id;
            let c_thread_id = c.thread_id;
            let body_text = c.body.clone();
            let response = (StatusCode::CREATED, Json(c)).into_response();
            emit_comment_notifications(
                &state,
                doc_id,
                c_thread_id,
                comment_id,
                ctx.user_id,
                &body_text,
                CommentWrite::Created,
                &explicit,
            )
            .await;
            notify_comment_change(&state, doc_id);
            response
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "create_reply");
            internal()
        }
    }
}

/// GET /api/docs/{doc_id}/comments
async fn list_comments(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_viewer(&req) {
        return r;
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    match comments.list(doc_id, q.include_resolved).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_comments");
            internal()
        }
    }
}

/// Reject mutating a comment that does not belong to `doc_id`. `require_doc_role`
/// only authorizes the caller on the path's doc; without this a member of doc A
/// could resolve/react-to/edit a comment in doc B by pairing A's id with B's
/// comment id (cross-document IDOR).
async fn ensure_comment_in_doc(
    comments: &std::sync::Arc<dyn knot_storage::CommentStore>,
    comment_id: Uuid,
    doc_id: Uuid,
) -> Result<(), Response> {
    match comments.get(comment_id).await {
        Ok(c) if c.doc_id == doc_id => Ok(()),
        Ok(_) | Err(CommentStoreError::NotFound) => {
            Err(json_err(StatusCode::NOT_FOUND, "comment.not_found", ""))
        }
        Err(e) => {
            tracing::error!(error=?e, "ensure_comment_in_doc");
            Err(internal())
        }
    }
}

/// POST /api/docs/{doc_id}/comments/{thread_id}/resolve
async fn resolve_thread(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, thread_id, doc_id).await {
        return r;
    }
    match comments.resolve(thread_id).await {
        Ok(()) => {
            notify_comment_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(CommentStoreError::NotFound) => json_err(
            StatusCode::NOT_FOUND,
            "comment.not_found",
            "thread not found or not a root",
        ),
        Err(e) => {
            tracing::error!(error=?e, "resolve_thread");
            internal()
        }
    }
}

/// POST /api/docs/{doc_id}/comments/{thread_id}/unresolve
async fn unresolve_thread(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, thread_id, doc_id).await {
        return r;
    }
    match comments.unresolve(thread_id).await {
        Ok(()) => {
            notify_comment_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(CommentStoreError::NotFound) => json_err(
            StatusCode::NOT_FOUND,
            "comment.not_found",
            "thread not found or not a root",
        ),
        Err(e) => {
            tracing::error!(error=?e, "unresolve_thread");
            internal()
        }
    }
}

/// POST /api/docs/{doc_id}/comments/{comment_id}/reactions
async fn add_reaction(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 256).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let body_req: ReactionBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if !emoji_allowed(&body_req.emoji) {
        return json_err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "comment.invalid_emoji",
            "emoji not in allow-list",
        );
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, comment_id, doc_id).await {
        return r;
    }
    match comments
        .add_reaction(comment_id, ctx.user_id, &body_req.emoji)
        .await
    {
        Ok(()) => {
            notify_comment_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error=?e, "add_reaction");
            internal()
        }
    }
}

/// DELETE /api/docs/{doc_id}/comments/{comment_id}/reactions/{emoji}
async fn remove_reaction(
    State(state): State<AppState>,
    Path((doc_id, comment_id, emoji)): Path<(Uuid, Uuid, String)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, comment_id, doc_id).await {
        return r;
    }
    match comments
        .remove_reaction(comment_id, ctx.user_id, &emoji)
        .await
    {
        Ok(()) => {
            notify_comment_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error=?e, "remove_reaction");
            internal()
        }
    }
}

/// PATCH /api/docs/{doc_id}/comments/{comment_id} — author only
async fn edit_comment(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: EditBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "");
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };

    // Fetch existing to check authorship.
    let existing = match comments.get(comment_id).await {
        Ok(c) => c,
        Err(CommentStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "edit_comment get");
            return internal();
        }
    };
    if existing.doc_id != doc_id {
        return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
    }
    if existing.author_id != ctx.user_id {
        return json_err(
            StatusCode::FORBIDDEN,
            "comment.not_author",
            "only the author can edit",
        );
    }

    match comments.update_body(comment_id, &body_req.body).await {
        Ok(c) => {
            let comment_id_val = c.id;
            let doc_id_val = c.doc_id;
            let thread_id_val = c.thread_id;
            let body_text = c.body.clone();
            let response = Json(c).into_response();
            emit_comment_notifications(
                &state,
                doc_id_val,
                thread_id_val,
                comment_id_val,
                ctx.user_id,
                &body_text,
                CommentWrite::Edited,
                &[],
            )
            .await;
            notify_comment_change(&state, doc_id);
            response
        }
        Err(CommentStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "comment.not_found", "")
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "edit_comment update");
            internal()
        }
    }
}

/// DELETE /api/docs/{doc_id}/comments/{comment_id} — author or workspace owner
async fn delete_comment(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };

    let existing = match comments.get(comment_id).await {
        Ok(c) => c,
        Err(CommentStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "delete_comment get");
            return internal();
        }
    };

    if existing.doc_id != doc_id {
        return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
    }
    // Allow if author OR workspace owner.
    let is_author = existing.author_id == ctx.user_id;
    let is_workspace_owner = ctx.role == WorkspaceRole::Owner;
    if !is_author && !is_workspace_owner {
        return json_err(
            StatusCode::FORBIDDEN,
            "comment.not_author",
            "only author or workspace owner can delete",
        );
    }

    match comments.delete(comment_id).await {
        Ok(()) => {
            notify_comment_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(error=?e, "delete_comment");
            internal()
        }
    }
}

// ---------------------------------------------------------------------------
// Router — mounted inside docs::router doc_id_routes so require_doc_role_mw applies
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/docs/{id}/comments",
            post(create_thread).get(list_comments),
        )
        .route(
            "/api/docs/{id}/comments/{thread_id}/replies",
            post(create_reply),
        )
        .route(
            "/api/docs/{id}/comments/{thread_id}/resolve",
            post(resolve_thread),
        )
        .route(
            "/api/docs/{id}/comments/{thread_id}/unresolve",
            post(unresolve_thread),
        )
        .route(
            "/api/docs/{id}/comments/{comment_id}/reactions",
            post(add_reaction),
        )
        .route(
            "/api/docs/{id}/comments/{comment_id}/reactions/{emoji}",
            delete(remove_reaction),
        )
        .route(
            "/api/docs/{id}/comments/{comment_id}",
            patch(edit_comment).delete(delete_comment),
        )
}
