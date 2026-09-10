//! Document storage — CRUD + tree ops.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool};
use thiserror::Error;
use uuid::Uuid;

use crate::audit;
use crate::invalidations;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub sort_key: String,
    pub icon: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
    /// When true, this doc is a template — excluded from the main tree
    /// listing and surfaced in the "New document" gallery instead.
    /// See Plan 36 (docs/superpowers/plans/2026-06-04-doc-templates.md).
    pub is_template: bool,
}

#[derive(Debug, Error)]
pub enum DocStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("cycle")]
    Cycle,
}

#[async_trait]
pub trait DocStore: Send + Sync + 'static {
    async fn list_alive(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError>;
    async fn get(&self, doc_id: Uuid) -> Result<Option<Document>, DocStoreError>;
    async fn create(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
        title: &str,
        sort_key: &str,
        created_by: Uuid,
    ) -> Result<Document, DocStoreError>;
    async fn rename(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        title: &str,
        icon: Option<&str>,
    ) -> Result<Document, DocStoreError>;
    async fn move_to(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        parent_id: Option<Uuid>,
        sort_key: &str,
    ) -> Result<Document, DocStoreError>;
    async fn archive(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<(), DocStoreError>;
    async fn restore(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<(), DocStoreError>;
    /// Returns siblings under `parent_id` in sort order, archived ones
    /// included -- they still hold their sort_key, and the UNIQUE index over
    /// (workspace_id, parent_id, sort_key) still counts them.
    async fn siblings(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
    ) -> Result<Vec<Document>, DocStoreError>;
    /// Returns the IDs of all descendants (children, grandchildren, ...) of
    /// `doc_id` within the given workspace. Excludes the doc itself.
    async fn descendant_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError>;
    /// Documents in the workspace flagged as templates. Returned in
    /// title order. Used to populate the "New document" gallery.
    async fn list_templates(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError>;
    /// Flip the `is_template` flag on a doc. Auditable; owners only at
    /// the route layer.
    async fn set_template(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        is_template: bool,
    ) -> Result<Document, DocStoreError>;
}

#[derive(Clone)]
pub struct PgDocStore {
    pool: PgPool,
}

impl PgDocStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type DocRow = (
    Uuid,
    Uuid,
    Option<Uuid>,
    String,
    String,
    Option<String>,
    Uuid,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    bool,
);
fn doc_from_row(r: DocRow) -> Document {
    Document {
        id: r.0,
        workspace_id: r.1,
        parent_id: r.2,
        title: r.3,
        sort_key: r.4,
        icon: r.5,
        created_by: r.6,
        created_at: r.7,
        updated_at: r.8,
        archived_at: r.9,
        is_template: r.10,
    }
}
/// Shared SELECT column list, spliced into query strings with `format!`.
///
/// sqlx 0.9 only accepts `&'static str` directly, so every query below wraps
/// its `format!` in `AssertSqlSafe`. That assertion holds because this is the
/// ONLY value interpolated into those strings — it is a compile-time constant,
/// never caller-supplied — and every runtime value remains a `$N` bind.
/// Keep it that way: interpolating anything else here would defeat the check.
const COLS: &str = "id, workspace_id, parent_id, title, sort_key, icon, created_by, created_at, updated_at, archived_at, is_template";

#[async_trait]
impl DocStore for PgDocStore {
    async fn list_alive(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError> {
        // Templates live in their own gallery (see list_templates) and
        // are intentionally excluded from the main doc-tree listing.
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND archived_at IS NULL AND NOT is_template
             ORDER BY parent_id NULLS FIRST, sort_key"
        )))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn list_templates(&self, workspace_id: Uuid) -> Result<Vec<Document>, DocStoreError> {
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND archived_at IS NULL AND is_template
             ORDER BY title"
        )))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn set_template(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        is_template: bool,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET is_template = $3, updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(is_template)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            if is_template {
                "doc.template.mark"
            } else {
                "doc.template.unmark"
            },
            "doc",
            doc.id,
        )
        .await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn get(&self, doc_id: Uuid) -> Result<Option<Document>, DocStoreError> {
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents WHERE id = $1"
        )))
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(doc_from_row))
    }

    async fn create(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
        title: &str,
        sort_key: &str,
        created_by: Uuid,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(parent_id)
        .bind(title)
        .bind(sort_key)
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_unique)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(created_by),
            "doc.create",
            "doc",
            doc.id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc.id, "create").await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn rename(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        title: &str,
        icon: Option<&str>,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET title = $3, icon = COALESCE($4, icon), updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(title)
        .bind(icon)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.rename",
            "doc",
            doc.id,
        )
        .await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn move_to(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
        parent_id: Option<Uuid>,
        sort_key: &str,
    ) -> Result<Document, DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        // Reject moves that would create a parent cycle: the destination
        // parent must not be the doc itself or one of its descendants. Done
        // inside the tx so it's correct under concurrency.
        if let Some(p) = parent_id {
            let creates_cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE sub AS (
                     SELECT id FROM documents WHERE id = $1
                     UNION ALL
                     SELECT d.id FROM documents d JOIN sub s ON d.parent_id = s.id
                 )
                 SELECT EXISTS(SELECT 1 FROM sub WHERE id = $2)",
            )
            .bind(doc_id)
            .bind(p)
            .fetch_one(&mut *tx)
            .await?;
            if creates_cycle {
                return Err(DocStoreError::Cycle);
            }
        }
        let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "UPDATE documents SET parent_id = $3, sort_key = $4, updated_at = now()
             WHERE workspace_id = $1 AND id = $2
             RETURNING {COLS}"
        )))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(parent_id)
        .bind(sort_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_unique)?
        .ok_or(DocStoreError::NotFound)?;
        let doc = doc_from_row(row);
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.move",
            "doc",
            doc.id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc.id, "tree-move").await?;
        tx.commit().await?;
        Ok(doc)
    }

    async fn archive(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<(), DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let n = sqlx::query(
            "UPDATE documents SET archived_at = now()
             WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(DocStoreError::NotFound);
        }
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.archive",
            "doc",
            doc_id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "archive").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn restore(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        actor: Uuid,
    ) -> Result<(), DocStoreError> {
        let mut tx = crate::begin(&self.pool).await?;
        let n = sqlx::query(
            "UPDATE documents SET archived_at = NULL
             WHERE workspace_id = $1 AND id = $2 AND archived_at IS NOT NULL",
        )
        .bind(workspace_id)
        .bind(doc_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(DocStoreError::NotFound);
        }
        audit::record_in_tx(
            &mut tx,
            workspace_id,
            Some(actor),
            "doc.restore",
            "doc",
            doc_id,
        )
        .await?;
        invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "restore").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn siblings(
        &self,
        workspace_id: Uuid,
        parent_id: Option<Uuid>,
    ) -> Result<Vec<Document>, DocStoreError> {
        // Archived rows are included on purpose: they keep their sort_key and
        // still count towards UNIQUE (workspace_id, parent_id, sort_key), so a
        // caller generating the next key must see them or it can regenerate a
        // key an archived sibling still holds.
        let rows = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM documents
             WHERE workspace_id = $1 AND parent_id IS NOT DISTINCT FROM $2
             ORDER BY sort_key"
        )))
        .bind(workspace_id)
        .bind(parent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(doc_from_row).collect())
    }

    async fn descendant_ids(&self, doc_id: Uuid) -> Result<Vec<Uuid>, DocStoreError> {
        // Recursive CTE descending from doc_id.
        let rows = sqlx::query_scalar::<_, Uuid>(
            "WITH RECURSIVE chain AS (
                 SELECT id FROM documents WHERE parent_id = $1
                 UNION ALL
                 SELECT d.id FROM documents d JOIN chain c ON d.parent_id = c.id
             )
             SELECT id FROM chain",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

fn map_unique(e: sqlx::Error) -> DocStoreError {
    match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => DocStoreError::Conflict,
        e => DocStoreError::Sqlx(e),
    }
}
