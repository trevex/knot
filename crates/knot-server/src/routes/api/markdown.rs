//! GET  /api/docs/{id}/markdown    → text/markdown export
//! POST /api/docs/{id}/markdown    → cold-import markdown as a y-update
//!
//! The room actor is the exclusive owner of the live `DocHandle`, so we only
//! ask it for an encoded state snapshot (`Event::ExportState`) and perform
//! the (potentially expensive) markdown serialization here in the handler
//! against a transient doc. This keeps the actor responsive for editor
//! traffic and avoids polluting the `Engine` trait with a markdown concern.
//!
//! For import we parse the markdown to a y-update in the handler (pure
//! transform) and hand the bytes to the room, which applies + persists +
//! fans out to local connections.
//!
//! Import takes `?mode=append` (the default) or `?mode=replace`. Append
//! hands the parsed bytes to `Event::ApplyUpdate`, which Yjs *merges* into
//! the live fragment — right for `from_template`, which always targets a
//! fresh doc, wrong for importing over a page that already has content
//! (you get both, concatenated). Replace goes through
//! `Event::ReplaceWithMarkdown`, which clears the fragment and applies in a
//! single transaction. The default stays `append` so existing callers are
//! unaffected; the doc page's import control asks for `replace`.

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{Engine, YrsEngine};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

/// Errors `refresh_markdown_and_index` can return. Distinguishing the
/// kinds lets callers decide whether to surface them: the export
/// endpoint maps every variant to 500, while the patch endpoint logs
/// and ignores because the index refresh is bookkeeping rather than the
/// critical path.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("server state missing rooms registry")]
    NoRooms,
    #[error("room acquire failed for {0}")]
    Acquire(Uuid),
    #[error("room actor unreachable for {0}")]
    RoomUnreachable(Uuid),
    #[error("room actor returned an error: {0}")]
    Actor(String),
    #[error("yrs apply: {0}")]
    Apply(String),
    #[error("markdown serialise: {0}")]
    Serialise(String),
}

/// Re-render markdown from the live doc state, write to the cache, and
/// re-run the task indexer. Used after any mutation that should be
/// reflected on `/tasks` (markdown export, full-doc import via
/// ApplyUpdate/ReplaceWithMarkdown, individual task patch).
///
/// `actor_id` is whoever's edit triggered this refresh, forwarded to the
/// task indexer so a fresh `task_assigned` notification can name an actor
/// and self-assignment can be suppressed. Pass `None` when the caller has
/// no edit to attribute (e.g. a plain export) or genuinely doesn't know.
///
/// Best-effort: cache-put + indexer failures are logged but never
/// propagated. The Result reports only the steps before the cache write
/// (state export + markdown serialise), because failures there mean
/// callers got no usable text to return.
pub async fn refresh_markdown_and_index(
    state: &AppState,
    doc_id: Uuid,
    actor_id: Option<Uuid>,
) -> Result<String, RefreshError> {
    refresh_markdown_inner(state, doc_id, true, actor_id).await
}

/// Export the doc to markdown WITHOUT re-running the task indexer.
/// Used by the from-template flow so cloning a template doesn't
/// trigger a write to the template's own task rows.
pub async fn export_markdown_only(state: &AppState, doc_id: Uuid) -> Result<String, RefreshError> {
    refresh_markdown_inner(state, doc_id, false, None).await
}

async fn refresh_markdown_inner(
    state: &AppState,
    doc_id: Uuid,
    reindex_tasks: bool,
    actor_id: Option<Uuid>,
) -> Result<String, RefreshError> {
    let rooms = state.rooms_v2.clone().ok_or(RefreshError::NoRooms)?;
    let room = rooms
        .acquire(doc_id)
        .await
        .map_err(|_| RefreshError::Acquire(doc_id))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    if room
        .tx
        .send(knot_crdt::Event::ExportState(tx))
        .await
        .is_err()
    {
        return Err(RefreshError::RoomUnreachable(doc_id));
    }
    let (state_bytes, seq) = match rx.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(RefreshError::Actor(format!("{e:?}"))),
        Err(_) => return Err(RefreshError::RoomUnreachable(doc_id)),
    };
    let engine = YrsEngine;
    let transient = engine.new_doc();
    if let Err(e) = engine.apply_update(&transient, &state_bytes) {
        return Err(RefreshError::Apply(format!("{e:?}")));
    }
    let text = match knot_markdown::to_markdown::serialise(&engine, &transient) {
        Ok(md) => md,
        Err(e) => return Err(RefreshError::Serialise(format!("{e:?}"))),
    };
    if let Some(cache) = state.markdown_cache.clone()
        && let Err(e) = cache.put(doc_id, seq, &text).await
    {
        tracing::warn!(error=?e, "md cache put failed");
    }
    if reindex_tasks && let (Some(tasks), Some(docs)) = (state.tasks.clone(), state.docs.clone()) {
        let extracted = knot_markdown::tasks::extract_tasks(&text);
        let inputs: Vec<knot_storage::DocTaskInput> = extracted
            .into_iter()
            .map(|t| knot_storage::DocTaskInput {
                item_index: t.item_index,
                text: t.text,
                assignee_user_id: t.assignee_user_id,
                checked: t.checked,
                due_at: t.due_at,
            })
            .collect();
        match docs.get(doc_id).await {
            Ok(Some(doc)) => {
                if let Err(e) = tasks
                    .upsert_for_doc(doc.workspace_id, doc_id, &inputs, actor_id)
                    .await
                {
                    tracing::warn!(error=?e, "task reindex failed");
                }
            }
            _ => tracing::warn!(%doc_id, "task reindex: doc not found"),
        }
    }
    Ok(text)
}

pub(super) async fn export_inline(
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
    // A plain export doesn't edit the doc, so there's no actor to
    // attribute a `task_assigned` notification to.
    let text = match refresh_markdown_and_index(&state, doc_id, None).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "md export refresh");
            return internal();
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
        .body(Body::from(text))
        .unwrap()
}

/// Query for [`import_inline`].
///
/// Deliberately typed as `Option<String>` rather than a serde enum: an enum
/// would make an unknown value fail deserialization, and axum would answer
/// with its own plain-text 400 instead of our JSON error envelope.
#[derive(serde::Deserialize)]
pub(super) struct ImportQuery {
    #[serde(default)]
    mode: Option<String>,
}

pub(super) async fn import_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ImportQuery>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == knot_storage::WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    // `append` is the default because it is what shipped: `from_template`
    // and any existing API client rely on the merge. Only the doc page's
    // import control asks for `replace`.
    let replace = match q.mode.as_deref() {
        None | Some("append") => false,
        Some("replace") => true,
        Some(_) => return json_err(StatusCode::BAD_REQUEST, "markdown.bad_mode", ""),
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };

    let body = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let text = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.not_utf8", ""),
    };

    // Parse markdown to a y-update via knot_markdown. The parse function
    // builds a fresh transient doc and hands us the initial state update
    // bytes; we drop the doc and pass the bytes to the room.
    let update_bytes = match knot_markdown::from_markdown::parse(&text) {
        Ok((_doc, bytes)) => bytes,
        Err(e) => {
            tracing::warn!(error=?e, "md import parse");
            return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.parse", "");
        }
    };

    let room = match rooms.acquire(doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "room acquire failed");
            return internal();
        }
    };
    // The two events reply over differently-typed channels (`EngineError`
    // vs `String`), so they cannot share one oneshot — normalise to
    // `Result<i64, String>` here.
    let applied: Result<i64, String> = if replace {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => return internal(),
        }
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ApplyUpdate {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(Ok(seq)) => Ok(seq),
            Ok(Err(e)) => Err(format!("{e:?}")),
            Err(_) => return internal(),
        }
    };

    match applied {
        Ok(_seq) => {
            let _ = refresh_markdown_and_index(&state, doc_id, Some(ctx.user_id)).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, %doc_id, replace, "md import apply");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.apply", "")
        }
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
