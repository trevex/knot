//! Task index for Plan 31's workspace todo view. Tasks are derived from the
//! markdown-cache contents of each doc — every time the cache refreshes, the
//! indexer re-extracts task items and calls `upsert_for_doc`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocTask {
    pub id: String,
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub item_index: i32,
    pub text: String,
    pub assignee_user_id: Option<Uuid>,
    pub checked: bool,
    pub completed_at: Option<DateTime<Utc>>,
    /// Optional "due by" timestamp extracted from a `knot://time/<iso>`
    /// link inside the task content with an explicit "by"/"due" cue.
    /// `None` when no such cue is present.
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input shape from the markdown extractor — id is derived from doc_id +
/// item_index by the store.
#[derive(Debug, Clone, Default)]
pub struct DocTaskInput {
    pub item_index: i32,
    pub text: String,
    pub assignee_user_id: Option<Uuid>,
    pub checked: bool,
    pub due_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, TaskStoreError>;

#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Replace the task set for `doc_id` with `items`. Rows that fell out
    /// of the new set are deleted. `completed_at` is preserved across
    /// re-indexing when the checked status doesn't change.
    ///
    /// `actor_id` is whoever's edit triggered the reindex; it becomes the
    /// actor on any `task_assigned` notification, and suppresses the
    /// notification when someone assigns a task to themselves.
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        actor_id: Option<Uuid>,
    ) -> Result<()>;

    /// All open (uncompleted) tasks for a user across the workspace.
    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
    ) -> Result<Vec<DocTask>>;

    /// All tasks in a doc, useful for the per-doc rendering surface.
    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>>;
}

pub struct PgTaskStore {
    pool: PgPool,
}

impl PgTaskStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn task_id(doc_id: Uuid, item_index: i32) -> String {
    format!("{doc_id}:{item_index}")
}

#[async_trait]
impl TaskStore for PgTaskStore {
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        actor_id: Option<Uuid>,
    ) -> Result<()> {
        let mut tx = crate::begin(&self.pool).await?;

        // Build the new ID set so we can prune rows that fell out.
        let new_ids: Vec<String> = items
            .iter()
            .map(|i| task_id(doc_id, i.item_index))
            .collect();

        // Delete rows for this doc that aren't in the new set. NOT IN with
        // an empty array would be a no-op, so handle that case explicitly.
        if new_ids.is_empty() {
            sqlx::query("DELETE FROM doc_tasks WHERE doc_id = $1")
                .bind(doc_id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM doc_tasks WHERE doc_id = $1 AND NOT (id = ANY($2))")
                .bind(doc_id)
                .bind(&new_ids)
                .execute(&mut *tx)
                .await?;
        }

        // Upsert each task. `completed_at` flips to NOW() when `checked`
        // transitions false → true, and to NULL on true → false.
        for item in items {
            let id = task_id(doc_id, item.item_index);
            sqlx::query(
                "INSERT INTO doc_tasks (id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, CASE WHEN $7 THEN NOW() ELSE NULL END, $8) \
                 ON CONFLICT (id) DO UPDATE SET \
                   text = EXCLUDED.text, \
                   assignee_user_id = EXCLUDED.assignee_user_id, \
                   checked = EXCLUDED.checked, \
                   due_at = EXCLUDED.due_at, \
                   completed_at = CASE \
                     WHEN doc_tasks.checked = EXCLUDED.checked THEN doc_tasks.completed_at \
                     WHEN EXCLUDED.checked THEN NOW() \
                     ELSE NULL \
                   END, \
                   updated_at = NOW()",
            )
            .bind(&id)
            .bind(workspace_id)
            .bind(doc_id)
            .bind(item.item_index)
            .bind(&item.text)
            .bind(item.assignee_user_id)
            .bind(item.checked)
            .bind(item.due_at)
            .execute(&mut *tx)
            .await?;

            // Notify a new assignee — but key on the task's *content*, not
            // its id. Ids are "<doc_id>:<item_index>" and every reorder
            // rewrites them, so an id-keyed notification would re-fire for
            // every assignee each time anyone moved a list item.
            if let Some(assignee) = item.assignee_user_id
                && Some(assignee) != actor_id
            {
                use sha2::{Digest, Sha256};
                let digest = Sha256::digest(item.text.as_bytes());
                let short = hex_prefix(&digest);
                sqlx::query(
                    "INSERT INTO notifications \
                       (workspace_id, user_id, actor_id, kind, doc_id, target_kind, target_id, dedupe_key, data) \
                     VALUES ($1, $2, $3, 'task_assigned', $4, 'task', $5, $6, $7) \
                     ON CONFLICT (user_id, dedupe_key) DO NOTHING",
                )
                .bind(workspace_id)
                .bind(assignee)
                .bind(actor_id)
                .bind(doc_id)
                .bind(&id)
                .bind(format!("task_assigned:{doc_id}:{short}:{assignee}"))
                .bind(serde_json::json!({ "text": item.text }))
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
    ) -> Result<Vec<DocTask>> {
        let rows: Vec<DocTask> = if include_completed {
            sqlx::query_as::<_, DocTask>(
                "SELECT id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at, created_at, updated_at \
                 FROM doc_tasks \
                 WHERE workspace_id = $1 AND assignee_user_id = $2 \
                 ORDER BY checked, due_at NULLS LAST, updated_at DESC",
            )
            .bind(workspace_id)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, DocTask>(
                "SELECT id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at, created_at, updated_at \
                 FROM doc_tasks \
                 WHERE workspace_id = $1 AND assignee_user_id = $2 AND completed_at IS NULL \
                 ORDER BY due_at NULLS LAST, updated_at DESC",
            )
            .bind(workspace_id)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>> {
        let rows: Vec<DocTask> = sqlx::query_as::<_, DocTask>(
            "SELECT id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at, created_at, updated_at \
             FROM doc_tasks WHERE doc_id = $1 ORDER BY item_index",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

/// First 8 bytes of a digest as lowercase hex — 16 characters, enough to
/// key a notification without carrying the whole hash in every row.
fn hex_prefix(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for DocTask {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(DocTask {
            id: row.try_get("id")?,
            workspace_id: row.try_get("workspace_id")?,
            doc_id: row.try_get("doc_id")?,
            item_index: row.try_get("item_index")?,
            text: row.try_get("text")?,
            assignee_user_id: row.try_get("assignee_user_id")?,
            checked: row.try_get("checked")?,
            completed_at: row.try_get("completed_at")?,
            due_at: row.try_get("due_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}
