//! knot server binary.
//!
//! Plan 2 wires layered config + observability + Postgres pool.
//! Plan 3 adds a clap subcommand router: `Serve` (default) and `Admin`.

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process;

use knot_server::admin;

use clap::{Parser, Subcommand};
use knot_config::Config;

#[derive(Parser)]
#[command(name = "knot-server", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP/WS server (default).
    Serve,
    /// Administrative commands.
    Admin(admin::AdminArgs),
    /// Run pending Postgres migrations and exit.
    Migrate,
}

#[tokio::main]
async fn main() {
    // Load .env from the current directory (and walk up). Silently OK if absent.
    // Existing process env always wins, so prod (k8s envFrom) is unaffected.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let cfg = match Config::load(std::env::var("KNOT_CONFIG").ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {e}");
            process::exit(2);
        }
    };

    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => run_server(cfg).await,
        Cmd::Admin(a) => match admin::run(cfg, a).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("admin: {e}");
                process::exit(1);
            }
        },
        Cmd::Migrate => {
            if cfg.database_url.is_empty() {
                eprintln!("migrate: KNOT_DATABASE_URL must be set");
                process::exit(2);
            }
            match knot_storage::connect(&cfg.database_url, 1).await {
                Ok(_) => {
                    eprintln!("migrate: ok");
                }
                Err(e) => {
                    eprintln!("migrate: {e}");
                    process::exit(1);
                }
            }
        }
    }
}

async fn run_server(cfg: Config) {
    let cfg = std::sync::Arc::new(cfg);
    // 2. Init observability (logging + optional OTLP; metrics).
    let otlp_provider = if cfg.tracing_enabled && !cfg.otlp_endpoint.is_empty() {
        match knot_obs::tracing::init_with_otlp(
            &cfg.log_level,
            &cfg.log_format,
            &cfg.otlp_endpoint,
            "knot-server",
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("tracing init: {e}");
                process::exit(2);
            }
        }
    } else {
        if let Err(e) = knot_obs::logging::init(&cfg.log_level, &cfg.log_format) {
            eprintln!("logging init: {e}");
            process::exit(2);
        }
        None
    };
    if let Err(e) = knot_obs::metrics::init(&cfg.metrics_addr) {
        tracing::warn!(error=?e, "metrics init failed; continuing without /metrics");
    }

    // 3a. OIDC discovery if enabled.
    let oidc = if cfg.oidc_enabled {
        match knot_auth::oidc::OidcClient::discover(
            &cfg.oidc_issuer,
            &cfg.oidc_client_id,
            &cfg.oidc_client_secret,
            &cfg.oidc_redirect_url,
            cfg.oidc_extra_audiences_list(),
        )
        .await
        {
            Ok(c) => {
                tracing::info!(issuer=%cfg.oidc_issuer, "OIDC client ready");
                Some(std::sync::Arc::new(c))
            }
            Err(e) => {
                tracing::error!(error=?e, "OIDC discovery failed");
                process::exit(2);
            }
        }
    } else {
        None
    };

    // 3. Connect to Postgres if configured.
    let pool = if !cfg.database_url.is_empty() {
        match knot_storage::connect(&cfg.database_url, cfg.db_max_connections).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!(error=?e, "database connect failed");
                process::exit(3);
            }
        }
    } else {
        tracing::warn!("KNOT_DATABASE_URL not set; running in-memory only");
        None
    };

    // 3b. CRDT bus + Rooms registry (only when a real pool exists).
    let (bus, rooms_v2, reindex_rx) = if let Some(pool) = pool.clone() {
        match knot_crdt::PgBus::connect(&cfg.database_url).await {
            Ok(b) => {
                let bus: std::sync::Arc<dyn knot_crdt::Bus> = std::sync::Arc::new(b);
                let updates: std::sync::Arc<dyn knot_storage::UpdatesStore> =
                    std::sync::Arc::new(knot_storage::PgUpdatesStore::new(pool.clone()));
                let snapshots: std::sync::Arc<dyn knot_storage::SnapshotStore> =
                    std::sync::Arc::new(knot_storage::PgSnapshotStore::new(pool.clone()));
                let policy = knot_crdt::SnapshotPolicy {
                    every_n: cfg.snapshot_every_n,
                    idle: std::time::Duration::from_secs(cfg.snapshot_idle_sec as u64),
                };
                // Channel feeding the reindex worker. Bounded so a
                // pathological burst can't pile up unbounded; if it
                // fills, the room actor's try_send drops the
                // notification (next applied update will refire).
                let (dirty_tx, dirty_rx) = tokio::sync::mpsc::channel::<uuid::Uuid>(4096);
                let rooms = std::sync::Arc::new(
                    knot_crdt::Rooms::new(
                        std::sync::Arc::new(knot_crdt::YrsEngine),
                        bus.clone(),
                        updates.clone(),
                        snapshots.clone(),
                        policy,
                        std::time::Duration::from_secs(cfg.room_idle_evict_sec as u64),
                    )
                    .with_dirty_tx(dirty_tx),
                );
                knot_crdt::spawn_gc(pool.clone(), snapshots, updates, cfg.snapshot_every_n);
                (Some(bus), Some(rooms), Some(dirty_rx))
            }
            Err(e) => {
                tracing::error!(error=?e, "PgBus connect failed");
                process::exit(2);
            }
        }
    } else {
        #[allow(clippy::type_complexity)]
        let none: (
            Option<std::sync::Arc<dyn knot_crdt::Bus>>,
            Option<std::sync::Arc<knot_crdt::Rooms>>,
            Option<tokio::sync::mpsc::Receiver<uuid::Uuid>>,
        ) = (None, None, None);
        none
    };

    // 3c. BoardRooms registry — wired to the same Bus as the document Rooms
    //     so board edits converge across pods via Postgres LISTEN/NOTIFY.
    let board_rooms = if let (Some(pool), Some(b)) = (pool.clone(), bus.clone()) {
        let store: std::sync::Arc<dyn knot_storage::BoardStore> =
            std::sync::Arc::new(knot_storage::PgBoardStore::new(pool.clone()));
        Some(std::sync::Arc::new(knot_crdt::BoardRooms::new(
            std::sync::Arc::new(knot_crdt::YrsEngine),
            store,
            b,
        )))
    } else {
        None
    };

    // 4. Build router.
    let state = match pool {
        Some(p) => {
            let mut s = knot_server::AppState::with_pool(p);
            s.session_key = cfg.session_key.clone().into_bytes();
            s.base_url = cfg.base_url.clone();
            s.cookie_secure = cfg.cookie_secure;
            s.oidc_enabled = cfg.oidc_enabled;
            s.oidc = oidc;
            s.config = cfg.clone();
            s.bus = bus;
            s.rooms_v2 = rooms_v2;
            s.board_rooms = board_rooms;

            // Optional S3 blob backend (default: Postgres bytea, already wired
            // by AppState::with_pool).
            if cfg.blob_backend == "s3" {
                if cfg.s3_bucket.is_empty() {
                    eprintln!("KNOT_S3_BUCKET is required when KNOT_BLOB_BACKEND=s3");
                    process::exit(2);
                }
                match knot_storage::S3Store::from_env(
                    cfg.s3_bucket.clone(),
                    cfg.s3_region.clone(),
                    cfg.s3_endpoint.clone(),
                    cfg.s3_prefix.clone(),
                ) {
                    Ok(store) => {
                        tracing::info!(
                            bucket = %cfg.s3_bucket,
                            endpoint = %cfg.s3_endpoint,
                            "blob backend: s3"
                        );
                        s.blob_store = Some(std::sync::Arc::new(store));
                    }
                    Err(e) => {
                        eprintln!("S3 backend: {e}");
                        process::exit(2);
                    }
                }
            } else {
                tracing::info!("blob backend: postgres");
            }

            s
        }
        None => {
            let mut s = knot_server::AppState::in_memory();
            s.config = cfg.clone();
            s
        }
    };
    if let Some(rx) = reindex_rx {
        knot_server::reindex::spawn(state.clone(), rx);
        tracing::info!("reindex worker spawned");
    }
    if let (Some(pool), Some(acl), Some(docs)) =
        (state.pool.clone(), state.acl.clone(), state.docs.clone())
    {
        let rooms_for_revoke = state.rooms_v2.clone();
        let on_invalidate: std::sync::Arc<dyn Fn(uuid::Uuid) + Send + Sync> =
            std::sync::Arc::new(move |doc_id| {
                if let Some(r) = rooms_for_revoke.clone() {
                    tokio::spawn(async move {
                        r.revoke_all_for_doc(doc_id).await;
                    });
                }
            });
        let _handle = knot_docs::spawn_listener(pool, acl, docs, on_invalidate);
        tracing::info!("acl listener spawned");
    }
    if let (Some(pool), Some(rooms)) = (state.pool.clone(), state.rooms_v2.clone()) {
        let _handle = knot_server::comments_listener::spawn(pool, rooms);
        tracing::info!("comments listener spawned");
    }
    if let Some(pool) = state.pool.clone() {
        let _handle = knot_server::notifications_sweep::spawn(pool);
        tracing::info!("notification sweep spawned");
    }

    // Token shared with every collab socket; cancelled on SIGTERM so they
    // send a clean 1001 Close and drain instead of being severed mid-rollout.
    let shutdown = state.shutdown.clone();
    let app = knot_server::router_with_state(state);

    // 5. Bind + serve.
    let listener = match tokio::net::TcpListener::bind(normalize_addr(&cfg.addr)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error=?e, addr=%cfg.addr, "bind failed");
            process::exit(4);
        }
    };
    tracing::info!(addr=%listener.local_addr().unwrap(), "listening");
    let serve_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown))
    .await;
    if let Err(e) = serve_result {
        tracing::error!(error=?e, "serve failed");
        process::exit(5);
    }

    // Normal (drained) exit: flush traces.
    tracing::info!("server stopped; flushing telemetry");
    if let Some(p) = otlp_provider {
        knot_obs::tracing::shutdown(p);
    }
}

/// Resolves when a shutdown signal (SIGTERM in k8s, Ctrl-C locally) arrives.
/// Cancels the collab token first so in-flight WebSocket sessions close
/// cleanly, then returns to let axum stop accepting and drain HTTP.
async fn shutdown_signal(shutdown: tokio_util::sync::CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining collab sockets");
    shutdown.cancel();
}

fn normalize_addr(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix(':')
        && port.parse::<u16>().is_ok()
    {
        return format!("0.0.0.0:{port}");
    }
    addr.to_string()
}
