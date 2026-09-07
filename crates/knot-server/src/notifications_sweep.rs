//! Periodic notification work: overdue tasks, and retention.
//!
//! Runs on every replica with no leader election. Both halves are safe to
//! run concurrently — the overdue emit is `ON CONFLICT DO NOTHING` against
//! `notifications_dedupe`, and the prune (delegated to
//! `PgNotificationStore::prune`) is idempotent by construction.

use chrono::{DateTime, Utc};
use knot_storage::{NotificationStore, NotificationStoreError, PgNotificationStore};
use sqlx::PgPool;
use tokio::task::JoinHandle;

const INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub due_emitted: u64,
    pub pruned: u64,
}

/// Emit `task_due` for every open, assigned, overdue task, then prune.
/// The dedupe key carries the date, so an overdue task notifies once a day
/// rather than once every fifteen minutes.
pub async fn run_once(pool: &PgPool, now: DateTime<Utc>) -> Result<SweepOutcome, sqlx::Error> {
    let emitted = sqlx::query(
        "INSERT INTO notifications \
           (workspace_id, user_id, actor_id, kind, doc_id, target_kind, target_id, dedupe_key, data) \
         SELECT t.workspace_id, t.assignee_user_id, NULL, 'task_due', t.doc_id, 'task', t.id, \
                'task_due:' || t.id || ':' || to_char($1::timestamptz, 'YYYY-MM-DD'), \
                jsonb_build_object('text', t.text, 'due_at', t.due_at) \
         FROM doc_tasks t \
         WHERE t.assignee_user_id IS NOT NULL \
           AND t.checked = false \
           AND t.due_at IS NOT NULL \
           AND t.due_at < $1 \
         ON CONFLICT (user_id, dedupe_key) DO NOTHING",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();

    // Retention is `PgNotificationStore::prune`'s rule, not a second copy of
    // it — the store already implements "read rows die after 90 days, all
    // rows after 180" for the inbox-mutation endpoints, and duplicating
    // that DELETE here would leave two places to keep in sync.
    // `prune` returns the store's own error type; unwrap to the `sqlx::Error`
    // this function's signature carries — `NotificationStoreError` only
    // ever wraps one.
    let pruned = match PgNotificationStore::new(pool.clone()).prune(now).await {
        Ok(n) => n,
        Err(NotificationStoreError::Sqlx(e)) => return Err(e),
    };

    if emitted > 0 {
        metrics::counter!("knot_notifications_emitted_total", "kind" => "task_due")
            .increment(emitted);
    }
    Ok(SweepOutcome {
        due_emitted: emitted,
        pruned,
    })
}

/// Spawn the 15-minute loop. One per process; every replica runs its own.
pub fn spawn(pool: PgPool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        // The first tick fires immediately; skip it so a rolling restart
        // doesn't have every pod sweep at once on boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match run_once(&pool, Utc::now()).await {
                Ok(out) => tracing::debug!(
                    due_emitted = out.due_emitted,
                    pruned = out.pruned,
                    "notification sweep"
                ),
                Err(e) => tracing::warn!(error=?e, "notification sweep failed"),
            }
        }
    })
}
