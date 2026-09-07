//! `/api/*` routes. Csrf + RequireSession layered here.

use axum::{Router, middleware};

use crate::AppState;
use crate::auth::{csrf_mw, require_session_mw};

pub mod blobs;
pub mod boards;
pub mod comments;
pub mod docs;
pub mod export_import;
pub mod grants;
pub mod history;
pub mod markdown;
pub mod notifications;
pub mod search;
pub mod shares;
pub mod tasks;
pub mod workspace;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .merge(workspace::router())
        .merge(docs::router(state))
        .merge(blobs::router())
        .merge(search::router())
        .merge(shares::router())
        .merge(boards::router())
        .merge(tasks::router())
        .merge(notifications::router())
        .merge(export_import::router())
        .layer(middleware::from_fn(csrf_mw))
        .layer(middleware::from_fn(require_session_mw))
}
