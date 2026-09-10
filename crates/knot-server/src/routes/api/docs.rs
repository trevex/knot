//! Documents API:
//! - GET    /api/docs            flat list (alive only)
//! - POST   /api/docs            body: {title?, parent_id?, after_id?}
//! - GET    /api/docs/{id}        metadata + effective_role
//!
//! PATCH/DELETE/move/restore land in T13/T14 (handlers stubbed below for
//! router shape; replaced in later tasks).

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use knot_storage::{Document, WorkspaceRole, sort_key_between};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole, require_doc_role_mw};
use crate::http_error::json_err;

#[derive(Serialize)]
struct DocResponse {
    id: String,
    workspace_id: String,
    parent_id: Option<String>,
    title: String,
    sort_key: String,
    icon: Option<String>,
    created_by: String,
    archived: bool,
    is_template: bool,
}

fn to_response(d: &Document) -> DocResponse {
    DocResponse {
        id: d.id.to_string(),
        workspace_id: d.workspace_id.to_string(),
        parent_id: d.parent_id.map(|u| u.to_string()),
        title: d.title.clone(),
        sort_key: d.sort_key.clone(),
        icon: d.icon.clone(),
        created_by: d.created_by.to_string(),
        archived: d.archived_at.is_some(),
        is_template: d.is_template,
    }
}

pub fn router(state: AppState) -> Router<AppState> {
    let doc_id_routes: Router<AppState> = Router::new()
        .route("/api/docs/{id}", get(get_one).patch(rename).delete(archive))
        .route("/api/docs/{id}/move", post(move_doc))
        .route("/api/docs/{id}/restore", post(restore))
        .route(
            "/api/docs/{id}/markdown",
            get(crate::routes::api::markdown::export_inline)
                .post(crate::routes::api::markdown::import_inline),
        )
        .route(
            "/api/docs/{id}/grants",
            get(crate::routes::api::grants::list_inline),
        )
        .route(
            "/api/docs/{id}/grants/{principal}",
            put(crate::routes::api::grants::put_inline)
                .delete(crate::routes::api::grants::delete_inline),
        )
        // History endpoints share the same :id param and ACL layer.
        .route(
            "/api/docs/{id}/history",
            get(crate::routes::api::history::list),
        )
        .route(
            "/api/docs/{id}/history/{seq}/markdown",
            get(crate::routes::api::history::preview_markdown),
        )
        .route(
            "/api/docs/{id}/history/{seq}/restore",
            post(crate::routes::api::history::restore),
        )
        .merge(crate::routes::api::comments::routes())
        .route("/api/docs/{id}/template", post(set_template_inline))
        .route(
            "/api/docs/from-template/{id}",
            post(create_from_template_inline),
        )
        .layer(middleware::from_fn_with_state(state, require_doc_role_mw));
    let list_routes: Router<AppState> = Router::new()
        .route("/api/docs", get(list).post(create))
        .route("/api/workspace/templates", get(list_templates));
    list_routes.merge(doc_id_routes)
}

async fn list(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.list_alive(ctx.workspace_id).await {
        Ok(list) => Json(list.iter().map(to_response).collect::<Vec<_>>()).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    title: Option<String>,
    parent_id: Option<Uuid>,
    after_id: Option<Uuid>,
}

#[tracing::instrument(skip_all, name = "docs.create")]
async fn create(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if ctx.role == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<CreateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let title = body.title.unwrap_or_else(|| "Untitled".into());

    let siblings = match docs.siblings(ctx.workspace_id, body.parent_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error=?e, "siblings");
            return internal();
        }
    };
    let (a, b) = match body.after_id {
        None => (None, siblings.first().map(|d| d.sort_key.as_str())),
        Some(aid) => {
            let i = siblings.iter().position(|d| d.id == aid);
            match i {
                Some(i) => (
                    Some(siblings[i].sort_key.as_str()),
                    siblings.get(i + 1).map(|d| d.sort_key.as_str()),
                ),
                None => (siblings.last().map(|d| d.sort_key.as_str()), None),
            }
        }
    };
    let sk = sort_key_between(a, b);

    match docs
        .create(ctx.workspace_id, body.parent_id, &title, &sk, ctx.user_id)
        .await
    {
        Ok(d) => (StatusCode::CREATED, Json(to_response(&d))).into_response(),
        // The generated sort_key collided with a sibling's (see move_to for
        // the same case). That is a client-visible conflict, not an internal
        // failure — retrying with a different position can succeed.
        Err(knot_storage::DocStoreError::Conflict) => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "create");
            internal()
        }
    }
}

async fn get_one(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let doc = match docs.get(doc_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, "get");
            return internal();
        }
    };
    #[derive(Serialize)]
    struct GetResponse {
        #[serde(flatten)]
        doc: DocResponse,
        effective_role: String,
    }
    Json(GetResponse {
        doc: to_response(&doc),
        effective_role: role.0.as_str().into(),
    })
    .into_response()
}

#[derive(Deserialize)]
struct PatchRequest {
    title: Option<String>,
    icon: Option<String>,
}

#[tracing::instrument(skip_all, name = "docs.rename")]
async fn rename(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<PatchRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let cur = match docs.get(doc_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, "rename get");
            return internal();
        }
    };
    let title = body.title.as_deref().unwrap_or(&cur.title);
    match docs
        .rename(
            ctx.workspace_id,
            doc_id,
            ctx.user_id,
            title,
            body.icon.as_deref(),
        )
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "rename");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct MoveRequest {
    parent_id: Option<Uuid>,
    after_id: Option<Uuid>,
    before_id: Option<Uuid>,
}

async fn move_doc(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<MoveRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };

    // Spec §6.1: `parent_id` in the request body is the *target* parent. An
    // explicit `null` (or omitted field, since serde collapses both to
    // `None`) moves the doc to the workspace root. `move_to` will surface
    // `DocStoreError::NotFound` if the document doesn't exist, so we skip a
    // pre-flight existence check here.
    let new_parent = body.parent_id;

    let siblings = match docs.siblings(ctx.workspace_id, new_parent).await {
        Ok(s) => s.into_iter().filter(|d| d.id != doc_id).collect::<Vec<_>>(),
        Err(e) => {
            tracing::error!(error=?e, "move siblings");
            return internal();
        }
    };
    let end_of_siblings = || (siblings.last().map(|d| d.sort_key.as_str()), None);
    let (a, b) = match (body.after_id, body.before_id) {
        (Some(aid), _) => match siblings.iter().position(|d| d.id == aid) {
            Some(i) => (
                Some(siblings[i].sort_key.as_str()),
                siblings.get(i + 1).map(|d| d.sort_key.as_str()),
            ),
            None => end_of_siblings(),
        },
        (_, Some(bid)) => match siblings.iter().position(|d| d.id == bid) {
            Some(i) => (
                i.checked_sub(1)
                    .and_then(|j| siblings.get(j))
                    .map(|d| d.sort_key.as_str()),
                Some(siblings[i].sort_key.as_str()),
            ),
            None => end_of_siblings(),
        },
        (None, None) => end_of_siblings(),
    };
    let sk = sort_key_between(a, b);

    match docs
        .move_to(ctx.workspace_id, doc_id, ctx.user_id, new_parent, &sk)
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(knot_storage::DocStoreError::Conflict) => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        Err(knot_storage::DocStoreError::Cycle) => {
            json_err(StatusCode::CONFLICT, "doc.move_cycle", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "move");
            internal()
        }
    }
}

#[tracing::instrument(skip_all, name = "docs.archive")]
async fn archive(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
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
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.archive(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "archive");
            internal()
        }
    }
}

async fn restore(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
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
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.restore(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "restore");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct SetTemplateRequest {
    is_template: bool,
}

#[tracing::instrument(skip_all, name = "docs.set_template")]
async fn set_template_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    // Templates are workspace-level affordances; only owners flip the flag.
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<SetTemplateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs
        .set_template(ctx.workspace_id, doc_id, ctx.user_id, body.is_template)
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "set_template");
            internal()
        }
    }
}

async fn list_templates(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.list_templates(ctx.workspace_id).await {
        Ok(list) => Json(list.iter().map(to_response).collect::<Vec<_>>()).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_templates");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct FromTemplateRequest {
    title: Option<String>,
    parent_id: Option<Uuid>,
}

/// Create a new doc whose content is a clean markdown-clone of the
/// template. Intentionally drops the source template's comments,
/// history, and CRDT lineage — templates exist to be instantiated, not
/// linked. ACL: read on the template (enforced by the route layer);
/// write on the destination parent is enforced inline below.
#[tracing::instrument(skip_all, name = "docs.from_template")]
async fn create_from_template_inline(
    State(state): State<AppState>,
    Path(template_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // The route layer gives us read access on the template; that's the
    // gate for "can use as a template". Writing into the workspace tree
    // requires editor+.
    if ctx.role == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<FromTemplateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };

    // Pull the template's current markdown via the shared
    // export-from-room path so we get exactly what the user sees.
    let md = match crate::routes::api::markdown::export_markdown_only(&state, template_id).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(error=?e, %template_id, "from_template: refresh");
            return internal();
        }
    };

    // Allocate a sort_key at the end of the destination parent's children.
    let siblings = match docs.siblings(ctx.workspace_id, body.parent_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error=?e, "from_template siblings");
            return internal();
        }
    };
    let sk = sort_key_between(siblings.last().map(|d| d.sort_key.as_str()), None);

    // Default title falls back to the template's title + " copy".
    let title = match body.title {
        Some(t) if !t.trim().is_empty() => t,
        _ => match docs.get(template_id).await {
            Ok(Some(d)) => format!("{} copy", d.title),
            _ => "Untitled".to_string(),
        },
    };

    let new_doc = match docs
        .create(ctx.workspace_id, body.parent_id, &title, &sk, ctx.user_id)
        .await
    {
        Ok(d) => d,
        Err(knot_storage::DocStoreError::Conflict) => {
            return json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "from_template create");
            return internal();
        }
    };

    // Import the template's markdown into the new doc via the room
    // actor (mirrors POST /api/docs/{id}/markdown).
    let update_bytes = match knot_markdown::from_markdown::parse(&md) {
        Ok((_doc, bytes)) => bytes,
        Err(e) => {
            tracing::warn!(error=?e, "from_template parse");
            return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.parse", "");
        }
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };
    let room = match rooms.acquire(new_doc.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, doc_id=%new_doc.id, "room acquire failed");
            return internal();
        }
    };
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
        Ok(Ok(_)) => (StatusCode::CREATED, Json(to_response(&new_doc))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error=?e, "from_template apply");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.apply", "")
        }
        Err(_) => internal(),
    }
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
