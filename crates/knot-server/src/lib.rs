//! knot server library — exports `router()` for tests + state for main.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use knot_auth::{Hasher, Throttle};
use knot_config::Config;
use knot_docs::AclCache;
use knot_storage::{
    BlobMeta, BlobStore, CommentStore, DocStore, GrantStore, MarkdownCacheStore, NotificationStore,
    PgBytesStore, PgCommentStore, PgDocStore, PgGrantStore, PgMarkdownCache, PgNotificationStore,
    PgSearchStore, PgSessionStore, PgShareTokenStore, PgUserStore, PgWorkspaceStore, Pool,
    SearchStore, SessionStore, ShareTokenStore, UserStore, WorkspaceStore,
};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

/// Max inbound collab/board WS message. CRDT updates and board ops are far
/// smaller; this bounds memory amplification from a malicious frame.
const MAX_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

pub mod admin;
pub mod auth;
pub mod board_room_shim;
pub mod comments_listener;
pub mod http_error;
pub mod metrics;
pub mod protocol;
pub mod reindex;
pub mod room;
pub mod routes;
pub mod security_headers;

use auth::SessionDeps;

#[derive(Clone)]
pub struct AppState {
    pub pool: Option<Pool>,
    pub users: Option<Arc<dyn UserStore>>,
    pub workspaces: Option<Arc<dyn WorkspaceStore>>,
    pub sessions: Option<Arc<dyn SessionStore>>,
    pub docs: Option<Arc<dyn DocStore>>,
    pub grants: Option<Arc<dyn GrantStore>>,
    pub acl: Option<Arc<AclCache>>,
    pub markdown_cache: Option<Arc<dyn MarkdownCacheStore>>,
    pub search: Option<Arc<dyn SearchStore>>,
    pub shares: Option<Arc<dyn ShareTokenStore>>,
    pub comments: Option<Arc<dyn CommentStore>>,
    pub blob_store: Option<Arc<dyn BlobStore>>,
    pub blob_meta: Option<Arc<BlobMeta>>,
    pub snapshots: Option<Arc<dyn knot_storage::SnapshotStore>>,
    pub rooms_v2: Option<Arc<knot_crdt::Rooms>>,
    pub bus: Option<Arc<dyn knot_crdt::Bus>>,
    pub boards: Option<Arc<dyn knot_storage::BoardStore>>,
    pub board_rooms: Option<Arc<knot_crdt::BoardRooms>>,
    pub tasks: Option<Arc<dyn knot_storage::TaskStore>>,
    pub notifications: Option<Arc<dyn NotificationStore>>,
    pub hasher: Arc<Hasher>,
    pub throttle: Arc<Throttle>,
    pub session_key: Vec<u8>,
    pub base_url: String,
    pub cookie_secure: bool,
    pub oidc_enabled: bool,
    pub oidc: Option<Arc<knot_auth::oidc::OidcClient>>,
    pub config: Arc<Config>,
    /// Cancelled on SIGTERM/SIGINT so in-flight collab sockets send a clean
    /// 1001 Close and the process can drain within the grace period instead
    /// of being SIGKILLed mid-rollout.
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl AppState {
    pub fn in_memory() -> Self {
        Self {
            pool: None,
            users: None,
            workspaces: None,
            sessions: None,
            docs: None,
            grants: None,
            acl: None,
            markdown_cache: None,
            search: None,
            shares: None,
            comments: None,
            blob_store: None,
            blob_meta: None,
            snapshots: None,
            rooms_v2: None,
            bus: None,
            boards: None,
            board_rooms: None,
            tasks: None,
            notifications: None,
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            cookie_secure: true,
            oidc_enabled: false,
            oidc: None,
            config: Arc::new(Config::default()),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Constructor used by `main` + integration tests when a real Postgres
    /// pool is available. Wires every storage trait to the corresponding
    /// `Pg*` impl so callers don't have to assemble them by hand. Caller is
    /// still responsible for setting `session_key`, `base_url`, and
    /// `oidc_enabled` from configuration.
    pub fn with_pool(pool: Pool) -> Self {
        let users: Arc<dyn UserStore> = Arc::new(PgUserStore::new(pool.clone()));
        let workspaces: Arc<dyn WorkspaceStore> = Arc::new(PgWorkspaceStore::new(pool.clone()));
        let sessions: Arc<dyn SessionStore> = Arc::new(PgSessionStore::new(pool.clone()));
        let docs: Arc<dyn DocStore> = Arc::new(PgDocStore::new(pool.clone()));
        let grants: Arc<dyn GrantStore> = Arc::new(PgGrantStore::new(pool.clone()));
        let acl = Arc::new(AclCache::new(
            workspaces.clone(),
            grants.clone(),
            docs.clone(),
        ));
        let markdown_cache: Arc<dyn MarkdownCacheStore> =
            Arc::new(PgMarkdownCache::new(pool.clone()));
        let search: Arc<dyn SearchStore> = Arc::new(PgSearchStore::new(pool.clone()));
        let shares: Arc<dyn ShareTokenStore> = Arc::new(PgShareTokenStore::new(pool.clone()));
        let comments: Arc<dyn CommentStore> = Arc::new(PgCommentStore::new(pool.clone()));
        let blob_store: Arc<dyn BlobStore> = Arc::new(PgBytesStore::new(pool.clone()));
        let blob_meta = Arc::new(BlobMeta::new(pool.clone()));
        let snapshots: Arc<dyn knot_storage::SnapshotStore> =
            Arc::new(knot_storage::PgSnapshotStore::new(pool.clone()));
        let boards: Arc<dyn knot_storage::BoardStore> =
            Arc::new(knot_storage::PgBoardStore::new(pool.clone()));
        let tasks: Arc<dyn knot_storage::TaskStore> =
            Arc::new(knot_storage::PgTaskStore::new(pool.clone()));
        let notifications: Arc<dyn NotificationStore> =
            Arc::new(PgNotificationStore::new(pool.clone()));
        Self {
            pool: Some(pool),
            users: Some(users),
            workspaces: Some(workspaces),
            sessions: Some(sessions),
            docs: Some(docs),
            grants: Some(grants),
            acl: Some(acl),
            markdown_cache: Some(markdown_cache),
            search: Some(search),
            shares: Some(shares),
            comments: Some(comments),
            blob_store: Some(blob_store),
            blob_meta: Some(blob_meta),
            snapshots: Some(snapshots),
            rooms_v2: None,
            bus: None,
            boards: Some(boards),
            board_rooms: None,
            tasks: Some(tasks),
            notifications: Some(notifications),
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            cookie_secure: true,
            oidc_enabled: false,
            oidc: None,
            config: Arc::new(Config::default()),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn session_deps(&self) -> Option<SessionDeps> {
        Some(SessionDeps {
            sessions: self.sessions.clone()?,
            workspaces: self.workspaces.clone()?,
            session_key: self.session_key.clone(),
        })
    }
}

/// In-memory router (used by tests + the spike main without DB).
pub fn router() -> Router {
    router_with_state(AppState::in_memory())
}

pub fn router_with_state(state: AppState) -> Router {
    let web_dist = std::env::var("KNOT_WEB_DIST").unwrap_or_else(|_| "/web/dist".into());
    let index_path = format!("{web_dist}/index.html");
    let spa = ServeDir::new(&web_dist)
        .append_index_html_on_directories(true)
        .not_found_service(ServeFile::new(&index_path));

    // WS routes: NO timeout / body-limit (long-lived, streamed).
    let collab = Router::new()
        .route("/collab/doc/{doc_id}", get(collab_upgrade))
        .route("/collab/board/{board_id}", get(collab_board_upgrade));

    // Everything else: request timeout + body-size limit.
    let api_and_pages = Router::new()
        .merge(routes::health::router())
        .merge(routes::auth::router())
        .merge(routes::public::router())
        .merge(routes::api::router(state.clone()))
        .fallback_service(spa)
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ));

    let mut r = collab.merge(api_and_pages);

    if let Some(deps) = state.session_deps() {
        r = r.layer(axum::middleware::from_fn_with_state(
            deps,
            auth::session_loader_mw,
        ));
    }

    r.layer(axum::middleware::from_fn_with_state(
        state.cookie_secure,
        crate::security_headers::set_security_headers,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .layer(axum::middleware::from_fn(crate::metrics::record))
    .with_state(state)
}

#[tracing::instrument(skip_all, name = "collab.upgrade", fields(doc_id = %doc_id))]
async fn collab_upgrade(
    Path(doc_id): Path<Uuid>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    req: axum::extract::Request,
) -> axum::response::Response {
    let Some(ctx) = req.extensions().get::<crate::auth::AuthContext>().cloned() else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "auth.session_required",
        )
            .into_response();
    };
    let acl = match state.acl.as_ref() {
        Some(a) => a.clone(),
        None => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    let can_write = match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        // Viewers may connect (read/hydrate) but cannot write; Owner/Editor can.
        Ok(Some(role)) => {
            use knot_storage::WorkspaceRole;
            matches!(role, WorkspaceRole::Owner | WorkspaceRole::Editor)
        }
        Ok(None) => return (axum::http::StatusCode::FORBIDDEN, "acl.no_grant").into_response(),
        Err(_) => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    let rooms = match state.rooms_v2.as_ref() {
        Some(r) => r.clone(),
        None => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    let shutdown = state.shutdown.clone();
    let ws = ws
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES);
    ws.on_upgrade(move |socket| async move {
        crate::room::serve(rooms, doc_id, socket, can_write, shutdown).await;
    })
    .into_response()
}

#[tracing::instrument(skip_all, name = "collab.board.upgrade", fields(board_id = %board_id))]
async fn collab_board_upgrade(
    Path(board_id): Path<Uuid>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    req: axum::extract::Request,
) -> axum::response::Response {
    let Some(ctx) = req.extensions().get::<crate::auth::AuthContext>().cloned() else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "auth.session_required",
        )
            .into_response();
    };
    let Some(boards) = state.boards.as_ref().cloned() else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
    };
    let Some(acl) = state.acl.as_ref().cloned() else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
    };
    // Resolve the parent doc and gate on its ACL (editor+ for v1).
    let board = match boards.get(board_id).await {
        Ok(b) => b,
        Err(_) => return (axum::http::StatusCode::NOT_FOUND, "board.not_found").into_response(),
    };
    match acl
        .effective_role(ctx.workspace_id, board.doc_id, ctx.user_id)
        .await
    {
        Ok(Some(role)) => {
            use knot_storage::WorkspaceRole;
            if !matches!(role, WorkspaceRole::Owner | WorkspaceRole::Editor) {
                return (axum::http::StatusCode::FORBIDDEN, "acl.no_grant").into_response();
            }
        }
        Ok(None) => return (axum::http::StatusCode::FORBIDDEN, "acl.no_grant").into_response(),
        Err(_) => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    }
    let Some(board_rooms) = state.board_rooms.as_ref().cloned() else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
    };
    let shutdown = state.shutdown.clone();
    let ws = ws
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES);
    ws.on_upgrade(move |socket| async move {
        crate::board_room_shim::serve(board_rooms, board_id, socket, shutdown).await;
    })
    .into_response()
}
