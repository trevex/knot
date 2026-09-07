//! Background worker that turns the room actor's dirty notifications
//! (one `Uuid` per successful persist) into batched task-index
//! refreshes.
//!
//! Why a worker, not an inline call from the room:
//!   - The room actor lives in `knot-crdt` and must not depend on
//!     `AppState` / storage handles tied to HTTP. Decoupling via an
//!     mpsc keeps the layering clean.
//!   - A single user editing a doc emits many updates per second; we
//!     don't want to re-parse markdown on every keystroke. The worker
//!     accumulates dirty doc-ids into a HashSet and flushes on a
//!     periodic tick.
//!
//! Failure handling: `refresh_markdown_and_index` failures are logged
//! and dropped. The next dirty notification for the same doc will
//! re-attempt; the task index is best-effort already (see
//! `routes::api::markdown`).

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::AppState;
use crate::routes::api::markdown::refresh_markdown_and_index;

/// Flush dirty doc-ids every `FLUSH_INTERVAL`. Snappy enough that the
/// `/tasks` page feels live, infrequent enough that a heavy typist
/// pays at most one extract per cycle.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Spawn the reindex worker. Returns immediately; the worker exits
/// when `rx` closes (i.e. the `Rooms` registry is dropped).
pub fn spawn(state: AppState, mut rx: mpsc::Receiver<Uuid>) {
    tokio::spawn(async move {
        let mut pending: HashSet<Uuid> = HashSet::new();
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        // `Burst` skip mode: if we miss a tick (heavy load), don't
        // try to catch up by firing back-to-back — just resume on the
        // next regular boundary.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(doc_id) => { pending.insert(doc_id); }
                    None => break,
                },
                _ = tick.tick() => {
                    if pending.is_empty() {
                        continue;
                    }
                    let to_flush = std::mem::take(&mut pending);
                    for doc_id in to_flush {
                        // The dirty-notification channel carries only a
                        // doc-id (see the module docs above), so there is
                        // no editor identity to forward here. That means a
                        // `task_assigned` notification triggered purely by
                        // this worker's periodic flush has no actor — it
                        // is never suppressed as a "self-assignment" and
                        // never attributed to anyone in particular.
                        if let Err(e) = refresh_markdown_and_index(&state, doc_id, None).await {
                            tracing::warn!(error=?e, %doc_id, "reindex worker: refresh failed");
                        }
                    }
                }
            }
        }
        tracing::info!("reindex worker stopped");
    });
}
