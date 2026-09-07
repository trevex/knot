//! In-app notification inbox.
//!
//! One table serves as inbox and outbox. Idempotency lives in the
//! `notifications_dedupe` unique index rather than in application logic:
//! every write is `ON CONFLICT DO NOTHING`, so concurrent emits from
//! several replicas converge on one row without coordination.
//!
//! `emailed_at` is untouched by this module. It exists so a future mailer
//! can poll `WHERE emailed_at IS NULL` without a migration.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Mention,
    Reply,
    TaskAssigned,
    TaskDue,
    DocShared,
}

impl NotificationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Reply => "reply",
            Self::TaskAssigned => "task_assigned",
            Self::TaskDue => "task_due",
            Self::DocShared => "doc_shared",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewNotification {
    pub workspace_id: Uuid,
    /// Recipient.
    pub user_id: Uuid,
    /// Who caused it. `None` for system events (`task_due`).
    pub actor_id: Option<Uuid>,
    pub kind: NotificationKind,
    pub doc_id: Option<Uuid>,
    pub target_kind: String,
    pub target_id: String,
    pub dedupe_key: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub doc_id: Option<Uuid>,
    pub target_kind: String,
    pub target_id: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub actor_id: Option<Uuid>,
    pub actor_display_name: Option<String>,
    pub doc_title: Option<String>,
}

#[derive(Debug, Error)]
pub enum NotificationStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, NotificationStoreError>;

/// Columns every read shares. `doc_title` is NULL for notifications with no
/// document (none today) and for archived documents, which still resolve.
const SELECT_COLS: &str = "n.id, n.kind, n.doc_id, n.target_kind, n.target_id, n.data, \
     n.created_at, n.read_at, n.actor_id, \
     u.display_name AS actor_display_name, d.title AS doc_title";

const FROM_JOINS: &str = "FROM notifications n \
     LEFT JOIN users u ON u.id = n.actor_id \
     LEFT JOIN documents d ON d.id = n.doc_id";

#[async_trait]
pub trait NotificationStore: Send + Sync + 'static {
    /// Insert one notification. Returns `false` when it was deduped or
    /// dropped as a self-notification — never an error in either case.
    async fn emit(&self, n: &NewNotification) -> Result<bool>;

    /// Insert several. Returns how many rows were actually created.
    async fn emit_many(&self, ns: &[NewNotification]) -> Result<usize>;

    /// Newest first. `cursor` is the last id of the previous page.
    async fn list(
        &self,
        user_id: Uuid,
        unread_only: bool,
        limit: i64,
        cursor: Option<i64>,
    ) -> Result<Vec<Notification>>;

    /// Unread rows, counted no further than `cap` so an ignored inbox
    /// cannot turn the polled endpoint into a sequential scan.
    async fn unread_count(&self, user_id: Uuid, cap: i64) -> Result<i64>;

    async fn mark_read(&self, user_id: Uuid, ids: &[i64]) -> Result<u64>;
    async fn mark_all_read(&self, user_id: Uuid) -> Result<u64>;

    /// Delete read rows older than 90 days and any row older than 180.
    async fn prune(&self, now: DateTime<Utc>) -> Result<u64>;
}

pub struct PgNotificationStore {
    pool: PgPool,
}

impl PgNotificationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Test-only: backdate a row and optionally mark it read, so `prune`
    /// can be exercised without waiting 90 days.
    pub async fn set_ages_for_test(&self, id: i64, days_old: i64, read: bool) -> Result<()> {
        sqlx::query(
            "UPDATE notifications \
             SET created_at = now() - ($2 || ' days')::interval, \
                 read_at = CASE WHEN $3 THEN now() - ($2 || ' days')::interval ELSE NULL END \
             WHERE id = $1",
        )
        .bind(id)
        .bind(days_old.to_string())
        .bind(read)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Test-only: hard-delete a document to prove the FK cascade.
    pub async fn hard_delete_doc_for_test(&self, doc_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM documents WHERE id = $1")
            .bind(doc_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl NotificationStore for PgNotificationStore {
    async fn emit(&self, n: &NewNotification) -> Result<bool> {
        if n.actor_id == Some(n.user_id) {
            return Ok(false);
        }
        let res = sqlx::query(
            "INSERT INTO notifications \
               (workspace_id, user_id, actor_id, kind, doc_id, target_kind, target_id, dedupe_key, data) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (user_id, dedupe_key) DO NOTHING",
        )
        .bind(n.workspace_id)
        .bind(n.user_id)
        .bind(n.actor_id)
        .bind(n.kind.as_str())
        .bind(n.doc_id)
        .bind(&n.target_kind)
        .bind(&n.target_id)
        .bind(&n.dedupe_key)
        .bind(&n.data)
        .execute(&self.pool)
        .await?;
        let created = res.rows_affected() == 1;
        if created {
            metrics::counter!("knot_notifications_emitted_total", "kind" => n.kind.as_str())
                .increment(1);
        }
        Ok(created)
    }

    async fn emit_many(&self, ns: &[NewNotification]) -> Result<usize> {
        let mut created = 0;
        for n in ns {
            if self.emit(n).await? {
                created += 1;
            }
        }
        Ok(created)
    }

    async fn list(
        &self,
        user_id: Uuid,
        unread_only: bool,
        limit: i64,
        cursor: Option<i64>,
    ) -> Result<Vec<Notification>> {
        let sql = format!(
            "SELECT {SELECT_COLS} {FROM_JOINS} \
             WHERE n.user_id = $1 \
               AND ($2 = false OR n.read_at IS NULL) \
               AND ($3::bigint IS NULL OR n.id < $3) \
             ORDER BY n.id DESC LIMIT $4"
        );
        let rows = sqlx::query_as::<_, Notification>(sqlx::AssertSqlSafe(sql))
            .bind(user_id)
            .bind(unread_only)
            .bind(cursor)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn unread_count(&self, user_id: Uuid, cap: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*)::bigint FROM \
               (SELECT 1 FROM notifications WHERE user_id = $1 AND read_at IS NULL LIMIT $2) t",
        )
        .bind(user_id)
        .bind(cap)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    async fn mark_read(&self, user_id: Uuid, ids: &[i64]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let res = sqlx::query(
            "UPDATE notifications SET read_at = now() \
             WHERE user_id = $1 AND id = ANY($2) AND read_at IS NULL",
        )
        .bind(user_id)
        .bind(ids)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn mark_all_read(&self, user_id: Uuid) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE notifications SET read_at = now() WHERE user_id = $1 AND read_at IS NULL",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn prune(&self, now: DateTime<Utc>) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM notifications \
             WHERE (read_at IS NOT NULL AND created_at < $1 - interval '90 days') \
                OR created_at < $1 - interval '180 days'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}
