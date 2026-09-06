# Notifications and Inbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give knot an in-app notification inbox — mentions, replies, task assignment, overdue tasks and document shares land in a per-user list with an unread badge.

**Architecture:** A single `notifications` table acts as both inbox and outbox: every source writes a row with a `dedupe_key` protected by a unique index, which makes emits idempotent across replicas without coordination. Delivery is polling — TanStack Query refetches an unread count every 30s — so no new transport or connection lifecycle is introduced. The doc-scoped `MSG_MENTION` plumbing reserved in June is deleted rather than completed, because an inbox must reach a user who has no document open.

**Tech Stack:** Rust (axum, sqlx 0.9, async-trait, metrics), PostgreSQL 18, React 19 + TanStack Query + Tailwind, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-06-notifications-inbox-design.md`

## Global Constraints

- **Dev Postgres must be running:** `make compose.up` before any `cargo` test. Tests use `knot_test_support::fresh_db()` — never testcontainers.
- **Rust gate for every task:** `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo fmt --all -- --check` must pass before commit.
- **Web gate for every web task:** `cd web && pnpm tsc --noEmit && pnpm lint` must pass before commit.
- **Test commands:** Rust `cargo nextest run --workspace --all-features`; web `cd web && pnpm test`; e2e `cd e2e && pnpm playwright test`.
- **Commits:** Conventional Commits. Every task ends in exactly one commit.
- **Any new table must be added** to the `expected` list in `crates/knot-storage/tests/migrations_apply.rs` (alphabetically sorted) or that test fails.
- **Notification kinds are exactly:** `mention | reply | task_assigned | task_due | doc_shared`.
- **Retention:** read rows deleted after 90 days, all rows after 180 days.
- **Unread count cap:** 100 (UI renders `99+`).
- **Poll interval:** 30_000 ms.

---

### Task 1: Notifications table and store

**Files:**

- Create: `migrations/<timestamp>_notifications.sql`
- Create: `crates/knot-storage/src/notifications.rs`
- Modify: `crates/knot-storage/src/lib.rs:3-20` (module list), `:22-42` (re-exports)
- Modify: `crates/knot-storage/Cargo.toml` (add `serde_json.workspace = true`)
- Modify: `crates/knot-storage/tests/migrations_apply.rs:27-46` (expected table list)
- Modify: `crates/knot-obs/src/metrics.rs` (describe the counter)
- Test: `crates/knot-storage/tests/notifications.rs`

**Interfaces:**

- Consumes: `knot_storage::begin`, `knot_test_support::fresh_db`.
- Produces:
  - `knot_storage::NotificationKind` — enum `{Mention, Reply, TaskAssigned, TaskDue, DocShared}`, `fn as_str(&self) -> &'static str`.
  - `knot_storage::NewNotification { workspace_id: Uuid, user_id: Uuid, actor_id: Option<Uuid>, kind: NotificationKind, doc_id: Option<Uuid>, target_kind: String, target_id: String, dedupe_key: String, data: serde_json::Value }`
  - `knot_storage::Notification { id: i64, kind: String, doc_id: Option<Uuid>, target_kind: String, target_id: String, data: serde_json::Value, created_at: DateTime<Utc>, read_at: Option<DateTime<Utc>>, actor_id: Option<Uuid>, actor_display_name: Option<String>, doc_title: Option<String> }`
  - `knot_storage::NotificationStore` trait with `emit`, `emit_many`, `list`, `unread_count`, `mark_read`, `mark_all_read`, `prune`.
  - `knot_storage::PgNotificationStore::new(pool: PgPool)`.
- [ ] **Step 1: Scaffold the migration**

```bash
make migrate.create NAME=notifications
```

Fill the created file with:

```sql
CREATE TABLE notifications (
  id            bigserial PRIMARY KEY,
  workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  actor_id      uuid NULL REFERENCES users(id) ON DELETE SET NULL,
  kind          text NOT NULL,
  doc_id        uuid NULL REFERENCES documents(id) ON DELETE CASCADE,
  target_kind   text NOT NULL,
  target_id     text NOT NULL,
  dedupe_key    text NOT NULL,
  data          jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  read_at       timestamptz NULL,
  emailed_at    timestamptz NULL
);

-- Idempotency: every emit is ON CONFLICT DO NOTHING against this index.
CREATE UNIQUE INDEX notifications_dedupe ON notifications(user_id, dedupe_key);
-- Inbox listing, newest first, keyset-paged on id.
CREATE INDEX notifications_inbox ON notifications(user_id, id DESC);
-- The polled badge query.
CREATE INDEX notifications_unread ON notifications(user_id) WHERE read_at IS NULL;
```

- [ ] **Step 2: Add the table to the migration characterisation test**

In `crates/knot-storage/tests/migrations_apply.rs`, insert `"notifications",` into `expected` between `"documents",` and `"sessions",` (the list is alphabetical).

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: FAIL — the expected list contains `notifications` but the migration has not been applied to a fresh DB yet by this test run. (If it passes immediately, the migration is already picked up — that is fine, continue.)

- [ ] **Step 4: Add the serde_json dependency**

In `crates/knot-storage/Cargo.toml`, under `[dependencies]`, after `serde.workspace = true`:

```toml
serde_json.workspace = true
```

- [ ] **Step 5: Write the failing store test**

Create `crates/knot-storage/tests/notifications.rs`:

```rust
//! Integration tests for `PgNotificationStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use knot_storage::{
    NewNotification, NotificationKind, NotificationStore, PgDocStore, PgNotificationStore,
    PgUserStore, PgWorkspaceStore, DocStore, UserStore, WorkspaceRole, WorkspaceStore,
    sort_key_between,
};
use uuid::Uuid;

/// Returns (store, workspace_id, doc_id, alice_id, bob_id).
async fn setup() -> (PgNotificationStore, Uuid, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users.create_local("alice@x.test", "Alice", "$h$").await.unwrap();
    let bob = users.create_local("bob@x.test", "Bob Smith", "$h$").await.unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store.add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    ws_store.add_member(ws.id, bob.id, WorkspaceRole::Editor).await.unwrap();
    let docs = PgDocStore::new(pool.clone());
    let sk = sort_key_between(None, None);
    let doc = docs.create(ws.id, None, "Doc", &sk, alice.id).await.unwrap();
    (PgNotificationStore::new(pool), ws.id, doc.id, alice.id, bob.id)
}

fn mention_for(ws: Uuid, doc: Uuid, recipient: Uuid, actor: Uuid, comment: Uuid) -> NewNotification {
    NewNotification {
        workspace_id: ws,
        user_id: recipient,
        actor_id: Some(actor),
        kind: NotificationKind::Mention,
        doc_id: Some(doc),
        target_kind: "comment".into(),
        target_id: comment.to_string(),
        dedupe_key: format!("mention:{comment}"),
        data: serde_json::json!({ "excerpt": "hello @Bob Smith" }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn emit_then_list_returns_the_row_with_actor_and_doc_title() {
    let (store, ws, doc, alice, bob) = setup().await;
    let comment = Uuid::new_v4();
    assert!(store.emit(&mention_for(ws, doc, bob, alice, comment)).await.unwrap());

    let rows = store.list(bob, false, 50, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "mention");
    assert_eq!(rows[0].actor_display_name.as_deref(), Some("Alice"));
    assert_eq!(rows[0].doc_title.as_deref(), Some("Doc"));
    assert!(rows[0].read_at.is_none());

    // Alice is the actor, not a recipient.
    assert!(store.list(alice, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn emit_is_idempotent_on_dedupe_key() {
    let (store, ws, doc, alice, bob) = setup().await;
    let comment = Uuid::new_v4();
    let n = mention_for(ws, doc, bob, alice, comment);

    assert!(store.emit(&n).await.unwrap(), "first emit inserts");
    assert!(!store.emit(&n).await.unwrap(), "second emit is deduped");
    assert_eq!(store.list(bob, false, 50, None).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn self_notification_is_dropped() {
    let (store, ws, doc, alice, _bob) = setup().await;
    let comment = Uuid::new_v4();
    // Alice mentions herself.
    assert!(!store.emit(&mention_for(ws, doc, alice, alice, comment)).await.unwrap());
    assert!(store.list(alice, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_count_caps_and_mark_read_clears() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store.emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4())).await.unwrap();
    }
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 3);

    let rows = store.list(bob, true, 50, None).await.unwrap();
    let first = rows[0].id;
    assert_eq!(store.mark_read(bob, &[first]).await.unwrap(), 1);
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 2);

    assert_eq!(store.mark_all_read(bob).await.unwrap(), 2);
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 0);
    // Reading does not delete.
    assert_eq!(store.list(bob, false, 50, None).await.unwrap().len(), 3);
    assert!(store.list(bob, true, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_read_cannot_touch_another_users_rows() {
    let (store, ws, doc, alice, bob) = setup().await;
    store.emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4())).await.unwrap();
    let bobs = store.list(bob, true, 50, None).await.unwrap();

    assert_eq!(store.mark_read(alice, &[bobs[0].id]).await.unwrap(), 0);
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_pages_backwards_on_the_cursor() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store.emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4())).await.unwrap();
    }
    let page1 = store.list(bob, false, 2, None).await.unwrap();
    assert_eq!(page1.len(), 2);
    let page2 = store.list(bob, false, 2, Some(page1[1].id)).await.unwrap();
    assert_eq!(page2.len(), 1);
    assert!(page2[0].id < page1[1].id, "ids descend across pages");
}

#[tokio::test(flavor = "multi_thread")]
async fn prune_drops_read_rows_past_ninety_days_and_everything_past_one_eighty() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store.emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4())).await.unwrap();
    }
    let rows = store.list(bob, false, 50, None).await.unwrap();
    // Row 0: read, 100 days old  -> pruned.
    // Row 1: unread, 100 days old -> kept.
    // Row 2: unread, 200 days old -> pruned.
    store.set_ages_for_test(rows[0].id, 100, true).await.unwrap();
    store.set_ages_for_test(rows[1].id, 100, false).await.unwrap();
    store.set_ages_for_test(rows[2].id, 200, false).await.unwrap();

    assert_eq!(store.prune(chrono::Utc::now()).await.unwrap(), 2);
    let left = store.list(bob, false, 50, None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, rows[1].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_doc_cascades_its_notifications() {
    let (store, ws, doc, alice, bob) = setup().await;
    store.emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4())).await.unwrap();
    store.hard_delete_doc_for_test(doc).await.unwrap();
    assert!(store.list(bob, false, 50, None).await.unwrap().is_empty());
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo nextest run -p knot-storage --test notifications`
Expected: FAIL to compile — `unresolved import knot_storage::NotificationStore`.

- [ ] **Step 7: Write the store**

Create `crates/knot-storage/src/notifications.rs`:

```rust
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
```

- [ ] **Step 8: Export from the crate root**

In `crates/knot-storage/src/lib.rs`, add `pub mod notifications;` to the module list (alphabetically, after `pub mod markdown_cache;`) and this re-export after the `markdown_cache` one:

```rust
pub use notifications::{
    NewNotification, Notification, NotificationKind, NotificationStore, NotificationStoreError,
    PgNotificationStore,
};
```

- [ ] **Step 9: Describe the metric**

In `crates/knot-obs/src/metrics.rs`, beside the other `describe_counter!` calls:

```rust
    describe_counter!(
        "knot_notifications_emitted_total",
        "Notifications written to the inbox, by kind"
    );
```

- [ ] **Step 10: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-storage --test notifications --test migrations_apply`
Expected: PASS — 8 notification tests plus the migration test.

- [ ] **Step 11: Lint**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check`
Expected: no output, exit 0.

- [ ] **Step 12: Commit**

```bash
git add migrations crates/knot-storage crates/knot-obs
git commit -m "feat(storage): notifications table and store

Idempotency lives in the notifications_dedupe unique index rather than in
application logic, so concurrent emits from several replicas converge on
one row without coordination."
```

---

### Task 2: Wire the store into AppState

**Files:**

- Modify: `crates/knot-server/src/lib.rs:14-16` (imports), `:42-72` (struct + `in_memory`), `:115-170` (`with_pool`)

**Interfaces:**

- Consumes: `knot_storage::{NotificationStore, PgNotificationStore}` from Task 1.
- Produces: `AppState.notifications: Option<Arc<dyn NotificationStore>>`, populated by `with_pool`, `None` in `in_memory`.
- [ ] **Step 1: Add the import**

In `crates/knot-server/src/lib.rs`, extend the existing `knot_storage::{…}` import list with `NotificationStore` and `PgNotificationStore`.

- [ ] **Step 2: Add the field**

In `pub struct AppState`, after `pub tasks: Option<Arc<dyn knot_storage::TaskStore>>,`:

```rust
    pub notifications: Option<Arc<dyn NotificationStore>>,
```

- [ ] **Step 3: Set it in both constructors**

In `in_memory()`, after `tasks: None,` add `notifications: None,`.

In `with_pool()`, after the `tasks` binding:

```rust
        let notifications: Arc<dyn NotificationStore> =
            Arc::new(PgNotificationStore::new(pool.clone()));
```

and in the returned struct, after `tasks: Some(tasks),` add `notifications: Some(notifications),`.

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p knot-server --all-features`
Expected: success. Any other `AppState { … }` literal in the workspace that fails here must gain the field too — fix each before continuing.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/lib.rs
git commit -m "feat(server): wire the notification store into AppState"
```

---

### Task 3: Mention and reply notifications on comments

Replaces `broadcast_mentions` and the `comment_mentions` channel. Recipients still come from the display-name regex in this task; Task 5 adds explicit ids.

**Files:**

- Modify: `crates/knot-server/src/routes/api/comments.rs:141-188` (delete `broadcast_mentions`, add `emit_comment_notifications`), and its two call sites in `create_thread` and `create_reply`
- Test: `crates/knot-server/tests/notifications_integration.rs` (create)

**Interfaces:**

- Consumes: `AppState.notifications` (Task 2), `CommentStore::list`, `WorkspaceStore::list_members`, `DocStore::get`.
- Produces: `async fn emit_comment_notifications(state: &AppState, doc_id: Uuid, thread_id: Uuid, comment_id: Uuid, author_id: Uuid, body: &str)` — private to the module; called after a comment commits.
- [ ] **Step 1: Write the failing integration test**

Create `crates/knot-server/tests/notifications_integration.rs`:

```rust
//! Integration: comment writes produce inbox rows for the right people.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{NotificationStore, WorkspaceRole};
use tower::ServiceExt;
use uuid::Uuid;

/// Seed: workspace + alice (owner) + bob (editor) + a doc owned by alice.
/// Returns (state, ws_id, doc_id, alice_id, bob_id).
async fn seeded() -> (AppState, Uuid, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash("hunter22").unwrap();
    let ws = s.workspaces.as_ref().unwrap().create("default", "W").await.unwrap();
    let alice = s.users.as_ref().unwrap()
        .create_local("alice@example.com", "Alice", &hash).await.unwrap();
    let bob = s.users.as_ref().unwrap()
        .create_local("bob@example.com", "Bob", &hash).await.unwrap();
    s.workspaces.as_ref().unwrap()
        .add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    s.workspaces.as_ref().unwrap()
        .add_member(ws.id, bob.id, WorkspaceRole::Editor).await.unwrap();
    let doc = s.docs.as_ref().unwrap()
        .create(ws.id, None, "Test Doc", "m", alice.id).await.unwrap();
    (s, ws.id, doc.id, alice.id, bob.id)
}

/// Log in as `email` and return the Cookie header value to replay.
async fn login(state: &AppState, email: &str) -> String {
    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "email": email, "password": "hunter22" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "login failed for {email}");
    res.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

fn csrf_from(cookie: &str) -> String {
    cookie
        .split("; ")
        .find_map(|c| c.strip_prefix("csrf="))
        .unwrap_or_default()
        .to_string()
}

async fn post_json(
    state: &AppState,
    cookie: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("cookie", cookie)
                .header("x-csrf-token", csrf_from(cookie))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test(flavor = "multi_thread")]
async fn mention_in_a_comment_notifies_the_mentioned_user_only() {
    let (state, _ws, doc, alice, bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "please look @Bob" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let notifications = state.notifications.as_ref().unwrap();
    let bobs = notifications.list(bob, false, 50, None).await.unwrap();
    assert_eq!(bobs.len(), 1);
    assert_eq!(bobs[0].kind, "mention");
    assert_eq!(bobs[0].doc_id, Some(doc));

    // The author gets nothing.
    assert!(notifications.list(alice, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_notifies_thread_participants_but_not_the_replier() {
    let (state, _ws, doc, alice, bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    // Alice opens a thread with no mention.
    let (_, thread) = post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "what do we think?" }),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap();

    // Bob replies.
    let (status, _) = post_json(
        &state,
        &bob_cookie,
        &format!("/api/docs/{doc}/comments/{thread_id}/replies"),
        serde_json::json!({ "body": "looks fine" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let notifications = state.notifications.as_ref().unwrap();
    let alices = notifications.list(alice, false, 50, None).await.unwrap();
    assert_eq!(alices.len(), 1);
    assert_eq!(alices[0].kind, "reply");
    assert!(notifications.list(bob, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mentioned_participant_gets_one_row_not_two() {
    let (state, _ws, doc, alice, bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    let (_, thread) = post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "opening" }),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap();

    // Bob replies AND mentions Alice — she is a participant and mentioned.
    post_json(
        &state,
        &bob_cookie,
        &format!("/api/docs/{doc}/comments/{thread_id}/replies"),
        serde_json::json!({ "body": "done @Alice" }),
    )
    .await;

    let rows = state.notifications.as_ref().unwrap()
        .list(alice, false, 50, None).await.unwrap();
    assert_eq!(rows.len(), 1, "mention wins; no duplicate reply row");
    assert_eq!(rows[0].kind, "mention");
    let _ = bob;
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p knot-server --test notifications_integration`
Expected: FAIL — `assert_eq!(bobs.len(), 1)` sees 0, because nothing writes notifications yet.

- [ ] **Step 3: Replace `broadcast_mentions`**

In `crates/knot-server/src/routes/api/comments.rs`, delete the whole `broadcast_mentions` function (the doc comment, the body, and the `pg_notify('comment_mentions', …)` spawn) and put this in its place. Keep `MENTION_RE` and `extract_mentions` — they are the fallback path.

```rust
/// Write inbox rows for a new comment: `mention` for everyone named in the
/// body, `reply` for the thread's other participants. A user who is both
/// gets the mention only.
///
/// Runs after the comment has committed, so a crash in between loses the
/// notification. That is the same at-most-once behaviour the previous
/// `pg_notify` had, and a trait object cannot join the caller's transaction.
async fn emit_comment_notifications(
    state: &AppState,
    doc_id: Uuid,
    thread_id: Uuid,
    comment_id: Uuid,
    author_id: Uuid,
    body: &str,
) {
    let (Some(notifications), Some(docs), Some(workspaces), Some(comments)) = (
        state.notifications.clone(),
        state.docs.clone(),
        state.workspaces.clone(),
        state.comments.clone(),
    ) else {
        return;
    };
    let Ok(Some(doc)) = docs.get(doc_id).await else {
        return;
    };

    // Mentioned users: match handles against member display names.
    let handles = extract_mentions(body);
    let mut mentioned: Vec<Uuid> = Vec::new();
    if !handles.is_empty() {
        let Ok(members) = workspaces.list_members(doc.workspace_id).await else {
            return;
        };
        mentioned = members
            .into_iter()
            .filter(|m| handles.contains(&m.display_name.to_lowercase()))
            .map(|m| m.user_id)
            .collect();
    }

    let excerpt: String = body.chars().take(140).collect();
    let base_data = serde_json::json!({
        "excerpt": excerpt,
        "doc_title": doc.title,
        "thread_id": thread_id.to_string(),
    });

    let mut batch: Vec<knot_storage::NewNotification> = mentioned
        .iter()
        .map(|&uid| knot_storage::NewNotification {
            workspace_id: doc.workspace_id,
            user_id: uid,
            actor_id: Some(author_id),
            kind: knot_storage::NotificationKind::Mention,
            doc_id: Some(doc_id),
            target_kind: "comment".into(),
            target_id: comment_id.to_string(),
            dedupe_key: format!("mention:{comment_id}"),
            data: base_data.clone(),
        })
        .collect();

    // Thread participants, minus the author and minus anyone already
    // receiving a mention for this comment.
    if let Ok(thread) = comments.list(doc_id, true).await {
        let mut seen: Vec<Uuid> = mentioned.clone();
        seen.push(author_id);
        for c in thread.into_iter().filter(|c| c.thread_id == thread_id) {
            if seen.contains(&c.author_id) {
                continue;
            }
            seen.push(c.author_id);
            batch.push(knot_storage::NewNotification {
                workspace_id: doc.workspace_id,
                user_id: c.author_id,
                actor_id: Some(author_id),
                kind: knot_storage::NotificationKind::Reply,
                doc_id: Some(doc_id),
                target_kind: "comment".into(),
                target_id: comment_id.to_string(),
                dedupe_key: format!("reply:{comment_id}"),
                data: base_data.clone(),
            });
        }
    }

    if batch.is_empty() {
        return;
    }
    if let Err(e) = notifications.emit_many(&batch).await {
        tracing::warn!(error=?e, %comment_id, "emit comment notifications");
    }
}
```

- [ ] **Step 4: Update the two call sites**

In `create_thread`, replace `broadcast_mentions(&state, doc_id, comment_id, &body_text).await;` with:

```rust
            emit_comment_notifications(&state, doc_id, c_thread_id, comment_id, ctx.user_id, &body_text)
                .await;
```

and capture the thread id alongside the comment id in the same `Ok(c)` arm:

```rust
        Ok(c) => {
            let comment_id = c.id;
            let c_thread_id = c.thread_id;
            let body_text = c.body.clone();
```

Apply the identical change in `create_reply` — find its `Ok(c)` arm and its `broadcast_mentions` call and mirror both edits.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-server --test notifications_integration --test comments_integration`
Expected: PASS. `comments_integration` must stay green — it covers the `@mention` extraction that is still in use.

- [ ] **Step 6: Lint**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 7: Commit**

```bash
git add crates/knot-server
git commit -m "feat(comments): write mention and reply notifications

Replaces the pg_notify('comment_mentions') call nothing ever listened to."
```

---

### Task 4: Doc-shared notifications on grants

**Files:**

- Modify: `crates/knot-server/src/routes/api/grants.rs` (the `put_inline` handler's success arm, around `:108-125`)
- Test: `crates/knot-server/tests/notifications_integration.rs` (append)

**Interfaces:**

- Consumes: `AppState.notifications`, `DocStore::get`.
- Produces: nothing new; a `doc_shared` row keyed `share:<doc_id>:<grantee_uuid>`.
- [ ] **Step 1: Write the failing test**

Append to `crates/knot-server/tests/notifications_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn granting_access_notifies_the_grantee() {
    let (state, _ws, doc, _alice, bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc}/grants/user:{bob}"))
                .header("cookie", &cookie)
                .header("x-csrf-token", csrf_from(&cookie))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "editor", "inherit": true }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let rows = state.notifications.as_ref().unwrap()
        .list(bob, false, 50, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "doc_shared");
    assert_eq!(rows[0].doc_id, Some(doc));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p knot-server --test notifications_integration granting_access`
Expected: FAIL — `rows.len()` is 0.

- [ ] **Step 3: Emit on a successful grant**

In `crates/knot-server/src/routes/api/grants.rs`, in `put_inline`, replace the success arm:

```rust
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
```

with:

```rust
        Ok(()) => {
            if let Some(rest) = principal.strip_prefix("user:")
                && let Ok(grantee) = Uuid::parse_str(rest)
            {
                emit_doc_shared(&state, doc_id, grantee, ctx.user_id).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
```

and add at the bottom of the file:

```rust
/// Tell the grantee a document was shared with them. Best-effort: a failure
/// here must not fail the grant that already committed.
async fn emit_doc_shared(state: &AppState, doc_id: Uuid, grantee: Uuid, actor: Uuid) {
    let (Some(notifications), Some(docs)) = (state.notifications.clone(), state.docs.clone())
    else {
        return;
    };
    let Ok(Some(doc)) = docs.get(doc_id).await else {
        return;
    };
    let n = knot_storage::NewNotification {
        workspace_id: doc.workspace_id,
        user_id: grantee,
        actor_id: Some(actor),
        kind: knot_storage::NotificationKind::DocShared,
        doc_id: Some(doc_id),
        target_kind: "document".into(),
        target_id: doc_id.to_string(),
        dedupe_key: format!("share:{doc_id}:{grantee}"),
        data: serde_json::json!({ "doc_title": doc.title }),
    };
    if let Err(e) = notifications.emit(&n).await {
        tracing::warn!(error=?e, %doc_id, "emit doc_shared");
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p knot-server --test notifications_integration`
Expected: PASS — all four tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add crates/knot-server
git commit -m "feat(grants): notify a user when a document is shared with them"
```

---

### Task 5: Explicit mention ids on the comment API

The backend half of spec §3. `@Christian Hüning` cannot be resolved from text; the client sends the ids it picked.

**Files:**

- Modify: `crates/knot-server/src/routes/api/comments.rs` (`CreateThreadBody`, `CreateReplyBody`, `emit_comment_notifications`, both call sites)
- Test: `crates/knot-server/tests/notifications_integration.rs` (append)

**Interfaces:**

- Consumes: `WorkspaceStore::list_members`.
- Produces: `emit_comment_notifications` gains a parameter — final signature:
  `async fn emit_comment_notifications(state: &AppState, doc_id: Uuid, thread_id: Uuid, comment_id: Uuid, author_id: Uuid, body: &str, explicit: &[Uuid])`.
  `POST /api/docs/{id}/comments` and `…/replies` accept an optional `mentions: [uuid]` field.
- [ ] **Step 1: Write the failing tests**

Append to `crates/knot-server/tests/notifications_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn explicit_mention_ids_reach_a_user_whose_name_has_a_space() {
    let (state, ws, doc, _alice, _bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    // A member the display-name regex can never match.
    let hash = state.hasher.hash("hunter22").unwrap();
    let carol = state.users.as_ref().unwrap()
        .create_local("carol@example.com", "Carol Danvers", &hash).await.unwrap();
    state.workspaces.as_ref().unwrap()
        .add_member(ws, carol.id, WorkspaceRole::Editor).await.unwrap();

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({
            "body": "over to you @Carol Danvers",
            "mentions": [carol.id.to_string()],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let rows = state.notifications.as_ref().unwrap()
        .list(carol.id, false, 50, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "mention");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_id_for_a_non_member_is_ignored() {
    let (state, _ws, doc, _alice, _bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    // A real user who is not a member of this workspace.
    let hash = state.hasher.hash("hunter22").unwrap();
    let outsider = state.users.as_ref().unwrap()
        .create_local("mallory@example.com", "Mallory", &hash).await.unwrap();

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hi", "mentions": [outsider.id.to_string()] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    assert!(
        state.notifications.as_ref().unwrap()
            .list(outsider.id, false, 50, None).await.unwrap().is_empty(),
        "a non-member must not be notified"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_comment_without_the_field_still_resolves_by_display_name() {
    let (state, _ws, doc, _alice, bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "ping @Bob" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        state.notifications.as_ref().unwrap()
            .list(bob, false, 50, None).await.unwrap().len(),
        1
    );
}
```

- [ ] **Step 2: Run to verify the first two fail**

Run: `cargo nextest run -p knot-server --test notifications_integration`
Expected: `explicit_mention_ids_reach_a_user_whose_name_has_a_space` FAILS (0 rows). The other two pass already.

- [ ] **Step 3: Accept the field**

In `crates/knot-server/src/routes/api/comments.rs`, add to both `CreateThreadBody` and `CreateReplyBody`:

```rust
    /// User ids the client's mention picker resolved. Preferred over the
    /// display-name regex, which cannot match a name containing a space.
    #[serde(default)]
    mentions: Vec<Uuid>,
```

- [ ] **Step 4: Use it in the emit path**

Change the signature and the mention-resolution block of `emit_comment_notifications`:

```rust
async fn emit_comment_notifications(
    state: &AppState,
    doc_id: Uuid,
    thread_id: Uuid,
    comment_id: Uuid,
    author_id: Uuid,
    body: &str,
    explicit: &[Uuid],
) {
```

and replace the block that starts `let handles = extract_mentions(body);` through the end of the `if !handles.is_empty() { … }` with:

```rust
    // Explicit ids from the picker win; the regex is the fallback for
    // clients that don't send them (and every comment written before this
    // shipped). Either way, membership decides — an id for a non-member is
    // dropped rather than trusted.
    let members = match workspaces.list_members(doc.workspace_id).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let mentioned: Vec<Uuid> = if explicit.is_empty() {
        let handles = extract_mentions(body);
        members
            .iter()
            .filter(|m| handles.contains(&m.display_name.to_lowercase()))
            .map(|m| m.user_id)
            .collect()
    } else {
        members
            .iter()
            .filter(|m| explicit.contains(&m.user_id))
            .map(|m| m.user_id)
            .collect()
    };
```

- [ ] **Step 5: Pass it at both call sites**

In `create_thread` and `create_reply`, add `&body_req.mentions` as the final argument to `emit_comment_notifications`. Note `body_req` is moved by the store call in some arms — bind `let explicit = body_req.mentions.clone();` before the store call and pass `&explicit`.

- [ ] **Step 6: Run to verify all pass**

Run: `cargo nextest run -p knot-server --test notifications_integration --test comments_integration`
Expected: PASS — seven notification tests plus the comments suite.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add crates/knot-server
git commit -m "feat(comments): accept explicit mention ids

The display-name regex captures @Christian and matches no member, so
anyone whose name contains a space was unmentionable. The picker knows
who it picked; it now says so. Regex kept for existing clients."
```

---

### Task 6: Task-assignment notifications inside the reindex

**Files:**

- Modify: `crates/knot-storage/src/tasks.rs:88-145` (`upsert_for_doc`)
- Test: `crates/knot-storage/tests/notifications.rs` (append)

**Interfaces:**

- Consumes: the `notifications` table (Task 1) directly — this write joins the transaction `upsert_for_doc` already opens, so it does not go through `NotificationStore`.
- Produces: `task_assigned` rows keyed `task_assigned:<doc_id>:<sha256(text)[..16]>:<assignee>`.
- Signature change: `upsert_for_doc` gains a trailing parameter `actor_id: Option<Uuid>` — the user whose edit triggered the reindex, `None` when unknown.
- [ ] **Step 1: Write the failing test**

Append to `crates/knot-storage/tests/notifications.rs`:

```rust
use knot_storage::{DocTaskInput, PgTaskStore, TaskStore};

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_notifies_once_and_survives_a_reorder() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone()).create("default", "W").await.unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users.create_local("alice@x.test", "Alice", "$h$").await.unwrap();
    let bob = users.create_local("bob@x.test", "Bob", "$h$").await.unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store.add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    ws_store.add_member(ws.id, bob.id, WorkspaceRole::Editor).await.unwrap();
    let docs = PgDocStore::new(pool.clone());
    let doc = docs
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    let tasks = PgTaskStore::new(pool.clone());
    let notifications = PgNotificationStore::new(pool.clone());

    let ship = DocTaskInput {
        item_index: 0,
        text: "ship it".into(),
        assignee_user_id: Some(bob.id),
        checked: false,
        due_at: None,
    };
    let review = DocTaskInput {
        item_index: 1,
        text: "review it".into(),
        assignee_user_id: Some(bob.id),
        checked: false,
        due_at: None,
    };

    tasks
        .upsert_for_doc(ws.id, doc.id, &[ship.clone(), review.clone()], Some(alice.id))
        .await
        .unwrap();
    assert_eq!(notifications.list(bob.id, false, 50, None).await.unwrap().len(), 2);

    // Swap the two items. Every task id changes; no new notifications.
    let swapped = vec![
        DocTaskInput { item_index: 0, ..review.clone() },
        DocTaskInput { item_index: 1, ..ship.clone() },
    ];
    tasks.upsert_for_doc(ws.id, doc.id, &swapped, Some(alice.id)).await.unwrap();
    assert_eq!(
        notifications.list(bob.id, false, 50, None).await.unwrap().len(),
        2,
        "reordering a list must not re-notify"
    );

    // Editing the text is a new task as far as the key is concerned.
    let edited = vec![DocTaskInput { text: "ship it today".into(), ..ship.clone() }];
    tasks.upsert_for_doc(ws.id, doc.id, &edited, Some(alice.id)).await.unwrap();
    assert_eq!(notifications.list(bob.id, false, 50, None).await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_to_yourself_notifies_nobody() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone()).create("default", "W").await.unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$").await.unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "mine".into(),
                assignee_user_id: Some(alice.id),
                checked: false,
                due_at: None,
            }],
            Some(alice.id),
        )
        .await
        .unwrap();

    assert!(
        PgNotificationStore::new(pool)
            .list(alice.id, false, 50, None).await.unwrap().is_empty()
    );
}
```

Add `#[derive(Clone)]` usage support: `DocTaskInput` already derives `Clone`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p knot-storage --test notifications`
Expected: FAIL to compile — `upsert_for_doc` takes 3 arguments, not 4.

- [ ] **Step 3: Extend the trait and implementation**

In `crates/knot-storage/src/tasks.rs`, change the trait method:

```rust
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
```

Mirror the signature on the `impl`. Inside, after the per-item `INSERT INTO doc_tasks … ON CONFLICT …` `.execute(&mut *tx).await?;`, add:

```rust
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
```

and at the bottom of the file:

```rust
/// First 8 bytes of a digest as lowercase hex — 16 characters, enough to
/// key a notification without carrying the whole hash in every row.
fn hex_prefix(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}
```

- [ ] **Step 4: Fix the callers**

Run `cargo check --workspace --all-features` and add the new argument at each call site. Expect `crates/knot-server/src/reindex.rs` (pass the user whose edit dirtied the doc if the worker has it, otherwise `None`) and `crates/knot-storage/tests/tasks.rs` (pass `None`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-storage`
Expected: PASS — including the pre-existing `tasks.rs` suite.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add crates
git commit -m "feat(tasks): notify a new assignee, keyed on task content

doc_tasks ids are '<doc_id>:<item_index>' and every reindex rewrites
them, so an id-keyed notification would re-fire on every reorder."
```

---

### Task 7: The sweep — overdue tasks and retention

**Files:**

- Create: `crates/knot-server/src/notifications_sweep.rs`
- Modify: `crates/knot-server/src/lib.rs` (module list, beside `pub mod comments_listener;`)
- Modify: `crates/knot-server/src/main.rs:274-276` (spawn beside the comments listener)
- Test: `crates/knot-server/tests/notifications_sweep.rs` (create)

**Interfaces:**

- Consumes: `PgNotificationStore::prune`, the `doc_tasks` and `notifications` tables.
- Produces:
  - `knot_server::notifications_sweep::run_once(pool: &PgPool, now: DateTime<Utc>) -> Result<SweepOutcome, sqlx::Error>`
  - `pub struct SweepOutcome { pub due_emitted: u64, pub pruned: u64 }`
  - `knot_server::notifications_sweep::spawn(pool: PgPool) -> JoinHandle<()>` — 15-minute interval.
- [ ] **Step 1: Write the failing test**

Create `crates/knot-server/tests/notifications_sweep.rs`:

```rust
//! The overdue-task sweep emits once per task per day and is safe to run
//! concurrently on every replica.

use chrono::{Duration, Utc};
use knot_server::notifications_sweep;
use knot_storage::{
    DocStore, DocTaskInput, NotificationStore, PgDocStore, PgNotificationStore, PgTaskStore,
    PgUserStore, PgWorkspaceStore, TaskStore, UserStore, WorkspaceRole, WorkspaceStore,
    sort_key_between,
};

#[tokio::test(flavor = "multi_thread")]
async fn overdue_task_emits_once_per_day_even_across_concurrent_sweeps() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone()).create("default", "W").await.unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$").await.unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$").await.unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store.add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    ws_store.add_member(ws.id, bob.id, WorkspaceRole::Editor).await.unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "overdue thing".into(),
                assignee_user_id: Some(bob.id),
                checked: false,
                due_at: Some(Utc::now() - Duration::days(1)),
            }],
            Some(alice.id),
        )
        .await
        .unwrap();

    let notifications = PgNotificationStore::new(pool.clone());
    // The assignment itself notified once; count from there.
    let before = notifications.list(bob.id, false, 50, None).await.unwrap().len();

    let now = Utc::now();
    // Two replicas sweeping at the same moment.
    let (a, b) = tokio::join!(
        notifications_sweep::run_once(&pool, now),
        notifications_sweep::run_once(&pool, now),
    );
    let total = a.unwrap().due_emitted + b.unwrap().due_emitted;
    assert_eq!(total, 1, "concurrent sweeps must produce exactly one row");

    let rows = notifications.list(bob.id, false, 50, None).await.unwrap();
    assert_eq!(rows.len(), before + 1);
    assert!(rows.iter().any(|r| r.kind == "task_due"));

    // Same day again: nothing new.
    let again = notifications_sweep::run_once(&pool, now).await.unwrap();
    assert_eq!(again.due_emitted, 0);

    // Tomorrow: one more.
    let tomorrow = notifications_sweep::run_once(&pool, now + Duration::days(1))
        .await
        .unwrap();
    assert_eq!(tomorrow.due_emitted, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checked_task_is_never_overdue() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone()).create("default", "W").await.unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$").await.unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$").await.unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store.add_member(ws.id, alice.id, WorkspaceRole::Owner).await.unwrap();
    ws_store.add_member(ws.id, bob.id, WorkspaceRole::Editor).await.unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "already done".into(),
                assignee_user_id: Some(bob.id),
                checked: true,
                due_at: Some(Utc::now() - Duration::days(3)),
            }],
            Some(alice.id),
        )
        .await
        .unwrap();

    let out = notifications_sweep::run_once(&pool, Utc::now()).await.unwrap();
    assert_eq!(out.due_emitted, 0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p knot-server --test notifications_sweep`
Expected: FAIL to compile — `notifications_sweep` does not exist.

- [ ] **Step 3: Write the sweep**

Create `crates/knot-server/src/notifications_sweep.rs`:

```rust
//! Periodic notification work: overdue tasks, and retention.
//!
//! Runs on every replica with no leader election. Both halves are safe to
//! run concurrently — the overdue emit is `ON CONFLICT DO NOTHING` against
//! `notifications_dedupe`, and the prune is idempotent by construction.

use chrono::{DateTime, Utc};
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

    let pruned = sqlx::query(
        "DELETE FROM notifications \
         WHERE (read_at IS NOT NULL AND created_at < $1 - interval '90 days') \
            OR created_at < $1 - interval '180 days'",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();

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
```

- [ ] **Step 4: Register the module and spawn it**

In `crates/knot-server/src/lib.rs`, beside `pub mod comments_listener;`:

```rust
pub mod notifications_sweep;
```

In `crates/knot-server/src/main.rs`, immediately after the comments-listener spawn block (`tracing::info!("comments listener spawned");`), inside the same `if let Some(pool) = …` scope that provides a pool:

```rust
        let _handle = knot_server::notifications_sweep::spawn(pool.clone());
        tracing::info!("notification sweep spawned");
```

If the surrounding block moved `pool`, clone it before the earlier use.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-server --test notifications_sweep`
Expected: PASS — both tests.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add crates/knot-server
git commit -m "feat(server): sweep overdue tasks and prune old notifications

Every replica runs it; the dedupe index makes concurrent sweeps
idempotent, so there is no leader election to operate."
```

---

### Task 8: HTTP endpoints

**Files:**

- Create: `crates/knot-server/src/routes/api/notifications.rs`
- Modify: `crates/knot-server/src/routes/api/mod.rs:20-31` (module + `.merge`)
- Test: `crates/knot-server/tests/notifications_integration.rs` (append)

**Interfaces:**

- Consumes: `AppState.notifications`, `AppState.acl` (`AclCache::effective_role`).
- Produces:
  - `GET /api/notifications?filter=unread|all&limit=<n>&cursor=<id>` → `{ "items": [NotificationRow], "next_cursor": <id|null> }`
  - `GET /api/notifications/unread_count` → `{ "count": n, "capped": bool }`
  - `POST /api/notifications/read` — body `{"ids":[…]}` or `{"all":true}` → 204
  - `NotificationRow { id: i64, kind: String, doc_id: Option<String>, doc_title: Option<String>, target_kind: String, target_id: String, actor_display_name: Option<String>, data: serde_json::Value, created_at: String, read: bool }`
- [ ] **Step 1: Write the failing tests**

Append to `crates/knot-server/tests/notifications_integration.rs`:

```rust
async fn get_json(state: &AppState, cookie: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test(flavor = "multi_thread")]
async fn inbox_lists_counts_and_marks_read() {
    let (state, _ws, doc, _alice, _bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hey @Bob" }),
    )
    .await;

    let (status, count) = get_json(&state, &bob_cookie, "/api/notifications/unread_count").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count["count"], 1);
    assert_eq!(count["capped"], false);

    let (status, list) = get_json(&state, &bob_cookie, "/api/notifications?filter=unread").await;
    assert_eq!(status, StatusCode::OK);
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "mention");
    assert_eq!(items[0]["doc_title"], "Test Doc");
    assert_eq!(items[0]["actor_display_name"], "Alice");
    assert_eq!(items[0]["read"], false);
    let id = items[0]["id"].as_i64().unwrap();

    let (status, _) = post_json(
        &state,
        &bob_cookie,
        "/api/notifications/read",
        serde_json::json!({ "ids": [id] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, count) = get_json(&state, &bob_cookie, "/api/notifications/unread_count").await;
    assert_eq!(count["count"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_is_per_user() {
    let (state, _ws, doc, _alice, _bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hey @Bob" }),
    )
    .await;

    // Alice sees nothing — the row belongs to Bob.
    let (_, list) = get_json(&state, &alice_cookie, "/api/notifications").await;
    assert!(list["items"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_for_a_doc_the_user_cannot_read_is_filtered_out() {
    let (state, _ws, doc, _alice, bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hey @Bob" }),
    )
    .await;
    assert_eq!(
        get_json(&state, &bob_cookie, "/api/notifications").await.1["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A notification pointing at a document Bob cannot read. Workspace
    // membership grants a role on every doc in that workspace
    // (knot_docs::acl::resolve), so the way to be unable to read a doc is
    // for it to live in another workspace — which is exactly the tenancy
    // guard at acl.rs:59-62, and the same `effective_role` -> None branch a
    // revoked grant produces.
    let other_ws = state.workspaces.as_ref().unwrap()
        .create("other", "Other").await.unwrap();
    let other_doc = state.docs.as_ref().unwrap()
        .create(other_ws.id, None, "Elsewhere", "m", bob).await.unwrap();
    state.notifications.as_ref().unwrap()
        .emit(&knot_storage::NewNotification {
            workspace_id: other_ws.id,
            user_id: bob,
            actor_id: None,
            kind: knot_storage::NotificationKind::DocShared,
            doc_id: Some(other_doc.id),
            target_kind: "document".into(),
            target_id: other_doc.id.to_string(),
            dedupe_key: format!("share:{}:{bob}", other_doc.id),
            data: serde_json::json!({}),
        })
        .await
        .unwrap();

    // Two rows exist for Bob; the endpoint returns only the readable one.
    assert_eq!(
        state.notifications.as_ref().unwrap()
            .list(bob, false, 50, None).await.unwrap().len(),
        2
    );
    let items = get_json(&state, &bob_cookie, "/api/notifications").await.1;
    let items = items["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the unreadable row is filtered, not 403'd");
    assert_eq!(items[0]["kind"], "mention");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p knot-server --test notifications_integration inbox_lists`
Expected: FAIL — 404, because the route does not exist.

- [ ] **Step 3: Write the route**

Create `crates/knot-server/src/routes/api/notifications.rs`:

```rust
//! The per-user notification inbox.
//!
//! GET  /api/notifications?filter=unread|all&limit=&cursor=  → { items, next_cursor }
//! GET  /api/notifications/unread_count                      → { count, capped }
//! POST /api/notifications/read  { ids: [] } | { all: true } → 204
//!
//! Rows are addressed to exactly one recipient, so ownership needs no join.
//! Document *access* is a separate question: a notification can outlive the
//! grant that made it visible, so rows carrying a `doc_id` are re-checked
//! against `effective_role` and filtered, never 403'd.

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;
const UNREAD_CAP: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/unread_count", get(unread_count))
        .route("/api/notifications/read", post(mark_read))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<i64>,
}

#[derive(Serialize)]
struct NotificationRow {
    id: i64,
    kind: String,
    doc_id: Option<String>,
    doc_title: Option<String>,
    target_kind: String,
    target_id: String,
    actor_display_name: Option<String>,
    data: serde_json::Value,
    created_at: String,
    read: bool,
}

#[derive(Serialize)]
struct ListResponse {
    items: Vec<NotificationRow>,
    next_cursor: Option<i64>,
}

#[derive(Serialize)]
struct CountResponse {
    count: i64,
    capped: bool,
}

#[derive(Deserialize)]
struct ReadBody {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    all: bool,
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(notifications), Some(acl)) = (state.notifications.clone(), state.acl.clone()) else {
        return internal();
    };
    let unread_only = q.filter.as_deref() == Some("unread");
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows = match notifications
        .list(ctx.user_id, unread_only, limit, q.cursor)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error=?e, "notifications list");
            return internal();
        }
    };
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };

    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        // Filter, don't fail: access can be revoked after the row is written.
        if let Some(doc_id) = r.doc_id {
            match acl.effective_role(ctx.workspace_id, doc_id, ctx.user_id).await {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(e) => {
                    tracing::error!(error=?e, "notifications acl check");
                    return internal();
                }
            }
        }
        items.push(NotificationRow {
            id: r.id,
            kind: r.kind,
            doc_id: r.doc_id.map(|d| d.to_string()),
            doc_title: r.doc_title,
            target_kind: r.target_kind,
            target_id: r.target_id,
            actor_display_name: r.actor_display_name,
            data: r.data,
            created_at: r.created_at.to_rfc3339(),
            read: r.read_at.is_some(),
        });
    }

    Json(ListResponse { items, next_cursor }).into_response()
}

async fn unread_count(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(notifications) = state.notifications.clone() else {
        return internal();
    };
    match notifications.unread_count(ctx.user_id, UNREAD_CAP).await {
        Ok(n) => Json(CountResponse {
            count: n,
            capped: n >= UNREAD_CAP,
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications unread_count");
            internal()
        }
    }
}

async fn mark_read(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(notifications) = state.notifications.clone() else {
        return internal();
    };
    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "bad_request", ""),
    };
    let body: ReadBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    let res = if body.all {
        notifications.mark_all_read(ctx.user_id).await
    } else {
        notifications.mark_read(ctx.user_id, &body.ids).await
    };
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications mark_read");
            internal()
        }
    }
}
```

- [ ] **Step 4: Register the router**

In `crates/knot-server/src/routes/api/mod.rs`, add `pub mod notifications;` beside the other module declarations and `.merge(notifications::router())` into the chain beside `.merge(tasks::router())`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-server --test notifications_integration`
Expected: PASS — all tests in the file.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo fmt --all -- --check
git add crates/knot-server
git commit -m "feat(api): notification list, unread count and mark-read"
```

---

### Task 9: Web API client and the unread badge

**Files:**

- Create: `web/src/lib/notifications.api.ts`
- Create: `web/src/features/notifications/useNotifications.ts`
- Modify: `web/src/features/workspace/WorkspaceHeader.tsx:1-2` (imports), `:38-46` (nav)
- Test: `web/src/features/notifications/useNotifications.test.ts`

**Interfaces:**

- Consumes: `apiFetch`, `ApiResult` from `web/src/lib/api.ts`.
- Produces:
  - `notificationsApi.list(filter, cursor?)`, `notificationsApi.unreadCount()`, `notificationsApi.markRead(ids)`, `notificationsApi.markAllRead()`
  - `type Notification` and `type NotificationList` in `notifications.api.ts`
  - `useUnreadCount()`, `useNotificationList(filter)`, `useMarkRead()` in `useNotifications.ts`
  - `formatBadge(count: number, capped: boolean): string` — exported for the unit test.
- [ ] **Step 1: Write the failing test**

Create `web/src/features/notifications/useNotifications.test.ts`:

```ts
import { describe, expect, it } from "vitest";

import { formatBadge } from "./useNotifications";

describe("formatBadge", () => {
  it("renders a plain count under the cap", () => {
    expect(formatBadge(3, false)).toBe("3");
  });

  it("renders 99+ once capped", () => {
    expect(formatBadge(100, true)).toBe("99+");
  });

  it("renders an empty string at zero so the badge can hide", () => {
    expect(formatBadge(0, false)).toBe("");
  });
});
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd web && pnpm test -- useNotifications`
Expected: FAIL — cannot resolve `./useNotifications`.

- [ ] **Step 3: Write the API client**

Create `web/src/lib/notifications.api.ts`:

```ts
import { apiFetch, type ApiResult } from "./api";

export type Notification = {
  id: number;
  kind: "mention" | "reply" | "task_assigned" | "task_due" | "doc_shared";
  doc_id: string | null;
  doc_title: string | null;
  target_kind: string;
  target_id: string;
  actor_display_name: string | null;
  data: Record<string, unknown>;
  created_at: string;
  read: boolean;
};

export type NotificationList = {
  items: Notification[];
  next_cursor: number | null;
};

export type UnreadCount = { count: number; capped: boolean };

export const notificationsApi = {
  async list(filter: "all" | "unread" = "all", cursor?: number): Promise<ApiResult<NotificationList>> {
    const qs = new URLSearchParams({ filter });
    if (cursor !== undefined) qs.set("cursor", String(cursor));
    return apiFetch<NotificationList>(`/api/notifications?${qs.toString()}`);
  },
  async unreadCount(): Promise<ApiResult<UnreadCount>> {
    return apiFetch<UnreadCount>("/api/notifications/unread_count");
  },
  async markRead(ids: number[]): Promise<ApiResult<void>> {
    return apiFetch<void>("/api/notifications/read", { method: "POST", body: { ids } });
  },
  async markAllRead(): Promise<ApiResult<void>> {
    return apiFetch<void>("/api/notifications/read", { method: "POST", body: { all: true } });
  },
};
```

- [ ] **Step 4: Write the hooks**

Create `web/src/features/notifications/useNotifications.ts`:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { notificationsApi, type NotificationList, type UnreadCount } from "../../lib/notifications.api";

/** Delivery is polling, not push — see the spec's §6. Window focus covers
 *  most of the perceived latency; this is the ceiling. */
export const POLL_INTERVAL_MS = 30_000;

export function formatBadge(count: number, capped: boolean): string {
  if (count <= 0) return "";
  return capped ? "99+" : String(count);
}

export function useUnreadCount() {
  return useQuery<UnreadCount>({
    queryKey: ["notifications", "unread_count"],
    queryFn: async () => {
      const res = await notificationsApi.unreadCount();
      if ("error" in res) throw new Error(res.error.message);
      return res.ok;
    },
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: true,
  });
}

export function useNotificationList(filter: "all" | "unread") {
  return useQuery<NotificationList>({
    queryKey: ["notifications", "list", filter],
    queryFn: async () => {
      const res = await notificationsApi.list(filter);
      if ("error" in res) throw new Error(res.error.message);
      return res.ok;
    },
  });
}

export function useMarkRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (ids: number[] | "all") => {
      const res = ids === "all"
        ? await notificationsApi.markAllRead()
        : await notificationsApi.markRead(ids);
      if ("error" in res) throw new Error(res.error.message);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
  });
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cd web && pnpm test -- useNotifications`
Expected: PASS — 3 tests.

- [ ] **Step 6: Add the badge to the sidebar**

In `web/src/features/workspace/WorkspaceHeader.tsx`, extend the lucide import to include `Bell`, add the hook import:

```tsx
import { useUnreadCount, formatBadge } from "../notifications/useNotifications";
```

Inside the component, before `return`:

```tsx
  const unread = useUnreadCount();
  const badge = unread.data ? formatBadge(unread.data.count, unread.data.capped) : "";
```

and add this `Link` as the first child of the `<nav>`, before the Tasks link:

```tsx
        <Link
          to="/notifications"
          data-testid="sidebar-inbox"
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <Bell size={14} aria-hidden /> Inbox
          {badge !== "" && (
            <span
              data-testid="inbox-badge"
              className="ml-auto min-w-[18px] px-1 h-[18px] rounded-full bg-accent text-accent-fg text-[11px] font-semibold inline-flex items-center justify-center"
            >
              {badge}
            </span>
          )}
        </Link>
```

- [ ] **Step 7: Typecheck, lint, commit**

```bash
cd web && pnpm tsc --noEmit && pnpm lint && cd ..
git add web/src
git commit -m "feat(web): notification API client and unread badge"
```

---

### Task 10: The inbox page and dropdown

**Files:**

- Create: `web/src/features/notifications/NotificationsPage.tsx`
- Create: `web/src/features/notifications/NotificationDropdown.tsx`
- Create: `web/src/features/notifications/notificationTarget.ts`
- Modify: `web/src/routes.tsx:18-20` (lazy import), `:67-70` (route)
- Modify: `web/src/features/workspace/WorkspaceHeader.tsx` (the Inbox row becomes a dropdown trigger)

**Interfaces:**

- Consumes: `useNotificationList`, `useMarkRead` (Task 9), `useNavigate` from react-router-dom.
- Produces:
  - `targetPath(n: Notification): string | null` in `notificationTarget.ts`, shared by the page and the dropdown.
  - default-exported `NotificationsPage`; route `/notifications`.
  - `NotificationDropdown` (named export), rendered from the sidebar.
- Test ids: `notifications-page`, `notification-row`, `notifications-filter-unread`, `notifications-filter-all`, `notifications-mark-all`, `notifications-dropdown`, `notifications-open-inbox`.
- [ ] **Step 1: Write the shared target helper**

Create `web/src/features/notifications/notificationTarget.ts`:

```ts
import type { Notification } from "../../lib/notifications.api";

export const KIND_LABEL: Record<Notification["kind"], string> = {
  mention: "mentioned you",
  reply: "replied in your thread",
  task_assigned: "assigned you a task",
  task_due: "task is overdue",
  doc_shared: "shared a document with you",
};

/** Where a notification takes you. A comment-anchored one appends
 *  ?thread=<id> so DocPage can open the sidebar on that thread. */
export function targetPath(n: Notification): string | null {
  if (!n.doc_id) return null;
  const thread = typeof n.data.thread_id === "string" ? n.data.thread_id : null;
  return thread ? `/doc/${n.doc_id}?thread=${thread}` : `/doc/${n.doc_id}`;
}
```

- [ ] **Step 2: Write the page**

Create `web/src/features/notifications/NotificationsPage.tsx`:

```tsx
/**
 * /notifications — the full inbox.
 *
 * Rows carrying a doc_id navigate to that document; a comment-anchored one
 * appends ?thread=<id> so the comment sidebar opens on the right thread.
 */
import { useState } from "react";
import { useNavigate } from "react-router-dom";

import type { Notification } from "../../lib/notifications.api";
import { KIND_LABEL, targetPath } from "./notificationTarget";
import { useMarkRead, useNotificationList } from "./useNotifications";

export default function NotificationsPage() {
  const [filter, setFilter] = useState<"all" | "unread">("all");
  const list = useNotificationList(filter);
  const markRead = useMarkRead();
  const nav = useNavigate();

  function open(n: Notification) {
    if (!n.read) markRead.mutate([n.id]);
    const path = targetPath(n);
    if (path) void nav(path);
  }

  const items = list.data?.items ?? [];

  return (
    <div data-testid="notifications-page" className="max-w-3xl mx-auto px-6 py-8">
      <div className="flex items-center gap-2 mb-4">
        <h1 className="text-lg font-semibold text-fg flex-1">Inbox</h1>
        <button
          type="button"
          data-testid="notifications-filter-all"
          onClick={() => setFilter("all")}
          className={`h-7 px-2 rounded text-[13px] ${filter === "all" ? "bg-muted text-fg" : "text-fg-muted hover:text-fg"}`}
        >
          All
        </button>
        <button
          type="button"
          data-testid="notifications-filter-unread"
          onClick={() => setFilter("unread")}
          className={`h-7 px-2 rounded text-[13px] ${filter === "unread" ? "bg-muted text-fg" : "text-fg-muted hover:text-fg"}`}
        >
          Unread
        </button>
        <button
          type="button"
          data-testid="notifications-mark-all"
          onClick={() => markRead.mutate("all")}
          className="h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted"
        >
          Mark all read
        </button>
      </div>

      {list.isPending && <p className="text-[13px] text-fg-muted">Loading…</p>}
      {!list.isPending && items.length === 0 && (
        <p className="text-[13px] text-fg-muted">Nothing here yet.</p>
      )}

      <ul className="flex flex-col gap-1">
        {items.map((n) => (
          <li key={n.id}>
            <button
              type="button"
              data-testid="notification-row"
              data-kind={n.kind}
              data-read={n.read ? "true" : "false"}
              onClick={() => open(n)}
              className="w-full text-left px-3 py-2 rounded hover:bg-muted transition-colors ease-swift duration-150"
            >
              <div className="text-[13px] text-fg">
                {!n.read && (
                  <span aria-hidden className="inline-block h-2 w-2 rounded-full bg-accent mr-2" />
                )}
                <strong className="font-semibold">{n.actor_display_name ?? "knot"}</strong>{" "}
                {KIND_LABEL[n.kind]}
                {n.doc_title ? <> in <span className="text-fg-muted">{n.doc_title}</span></> : null}
              </div>
              {typeof n.data.excerpt === "string" && (
                <div className="text-[12px] text-fg-muted truncate">{n.data.excerpt}</div>
              )}
              <div className="text-[11px] text-fg-muted/80">
                {new Date(n.created_at).toLocaleString()}
              </div>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
```

- [ ] **Step 3: Register the route**

In `web/src/routes.tsx`, beside the other lazy imports:

```tsx
const NotificationsPage = lazy(() => import("./features/notifications/NotificationsPage"));
```

and in the `AppShell` children array, beside the `tasks` route:

```tsx
          { path: "notifications", element: <Lazy><NotificationsPage /></Lazy> },
```

- [ ] **Step 4: Write the dropdown**

Create `web/src/features/notifications/NotificationDropdown.tsx`:

```tsx
/**
 * The sidebar's Inbox dropdown — the last 10, unread first by recency.
 * Clicking a row marks it read and navigates; "Open inbox" goes to the
 * full page.
 */
import { useNavigate } from "react-router-dom";

import type { Notification } from "../../lib/notifications.api";
import { KIND_LABEL, targetPath } from "./notificationTarget";
import { useMarkRead, useNotificationList } from "./useNotifications";

export function NotificationDropdown({ onClose }: { onClose: () => void }) {
  const list = useNotificationList("all");
  const markRead = useMarkRead();
  const nav = useNavigate();

  function open(n: Notification) {
    if (!n.read) markRead.mutate([n.id]);
    const path = targetPath(n);
    onClose();
    if (path) void nav(path);
  }

  const items = (list.data?.items ?? []).slice(0, 10);

  return (
    <div
      data-testid="notifications-dropdown"
      className="absolute left-2 right-2 top-full z-50 mt-1 rounded-md border border-border bg-surface shadow-lg overflow-hidden"
    >
      {items.length === 0 && (
        <p className="px-3 py-3 text-[12px] text-fg-muted m-0">Nothing here yet.</p>
      )}
      <ul className="m-0 p-0 list-none max-h-[320px] overflow-y-auto">
        {items.map((n) => (
          <li key={n.id}>
            <button
              type="button"
              data-testid="notification-row"
              data-kind={n.kind}
              data-read={n.read ? "true" : "false"}
              onClick={() => open(n)}
              className="w-full text-left px-3 py-2 text-[12px] text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              {!n.read && (
                <span aria-hidden className="inline-block h-2 w-2 rounded-full bg-accent mr-2" />
              )}
              <strong className="font-semibold">{n.actor_display_name ?? "knot"}</strong>{" "}
              {KIND_LABEL[n.kind]}
              {n.doc_title ? <span className="text-fg-muted"> · {n.doc_title}</span> : null}
            </button>
          </li>
        ))}
      </ul>
      <button
        type="button"
        data-testid="notifications-open-inbox"
        onClick={() => {
          onClose();
          void nav("/notifications");
        }}
        className="w-full text-left px-3 py-2 text-[12px] text-fg-muted hover:text-fg hover:bg-muted border-t border-border"
      >
        Open inbox
      </button>
    </div>
  );
}
```

- [ ] **Step 5: Hang it off the sidebar Inbox row**

In `web/src/features/workspace/WorkspaceHeader.tsx`, replace the Inbox `<Link>` added in Task 9 with a button that toggles the dropdown. Keep the `sidebar-inbox` and `inbox-badge` test ids exactly as they were. Add to the imports:

```tsx
import { NotificationDropdown } from "../notifications/NotificationDropdown";
```

add `import { useState } from "react";` if the file does not already import it, add `const [inboxOpen, setInboxOpen] = useState(false);` alongside the other component state, wrap the `<nav>` in `<div className="relative">`, and use:

```tsx
        <button
          type="button"
          data-testid="sidebar-inbox"
          onClick={() => setInboxOpen((v) => !v)}
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <Bell size={14} aria-hidden /> Inbox
          {badge !== "" && (
            <span
              data-testid="inbox-badge"
              className="ml-auto min-w-[18px] px-1 h-[18px] rounded-full bg-accent text-accent-fg text-[11px] font-semibold inline-flex items-center justify-center"
            >
              {badge}
            </span>
          )}
        </button>
        {inboxOpen && <NotificationDropdown onClose={() => setInboxOpen(false)} />}
```

- [ ] **Step 6: Verify it builds**

Run: `cd web && pnpm tsc --noEmit && pnpm lint && pnpm build`
Expected: all three succeed.

- [ ] **Step 7: Commit**

```bash
git add web/src
git commit -m "feat(web): the inbox dropdown and /notifications page"
```

---

### Task 11: Comment mentions carry user ids

The frontend half of spec §3. `MentionPicker` is a **hook**
(`useMentionPicker(value, onChange)`) returning `{ textareaProps, picker }`,
not a component — the ids are threaded through it and out via
`CommentComposer`.

**Files:**

- Modify: `web/src/features/comments/MentionPicker.tsx:52-58` (`pick`), `:62-88` (`textareaProps`), `:91-110` (picker rows)
- Modify: `web/src/features/comments/CommentComposer.tsx` (accumulate ids, widen `onSubmit`)
- Modify: `web/src/features/comments/CommentSidebar.tsx:65-80` (pass them to the mutation)
- Modify: `web/src/lib/comments.api.ts:22+` (`createThread`, `createReply`)
- Test: `web/src/features/comments/CommentComposer.test.tsx` (create)

**Interfaces:**

- Consumes: `workspaceApi.listMembers` (already used by the hook).
- Produces:
  - `useMentionPicker(value: string, onChange: (v: string) => void, onPickUser?: (userId: string) => void)`
  - `CommentComposer` prop `onSubmit: (body: string, mentions: string[]) => void`
  - `commentsApi.createThread(docId, body, positionY, positionYEnd, anchorText, mentions)` and `commentsApi.createReply(docId, threadId, body, mentions)`
  - `data-testid="mention-item"` on each picker row.
- [ ] **Step 1: Write the failing test**

Create `web/src/features/comments/CommentComposer.test.tsx`:

```tsx
import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { CommentComposer } from "./CommentComposer";

const CAROL = {
  user_id: "11111111-1111-1111-1111-111111111111",
  display_name: "Carol Danvers",
  email: "carol@x.test",
  role: "editor",
};

vi.mock("../workspace/workspace.api", () => ({
  workspaceApi: { listMembers: vi.fn(async () => ({ ok: [CAROL] })) },
}));

afterEach(cleanup);

function renderComposer() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onSubmit = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <CommentComposer
        onSubmit={onSubmit}
        data-testid-input="composer-input"
        data-testid-submit="composer-submit"
      />
    </QueryClientProvider>,
  );
  return { onSubmit };
}

describe("CommentComposer mentions", () => {
  it("submits the picked member's id, not just the typed text", async () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId("composer-input") as HTMLTextAreaElement;

    // Type "@Car" and put the caret at the end so the hook parses a mention.
    fireEvent.change(textarea, { target: { value: "@Car" } });
    textarea.selectionStart = 4;
    fireEvent.keyUp(textarea);

    const row = await screen.findByTestId("mention-item");
    fireEvent.mouseDown(row);

    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("@Carol Danvers", [CAROL.user_id]);
  });

  it("submits an empty id list when nobody was picked", () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId("composer-input");
    fireEvent.change(textarea, { target: { value: "no mentions here" } });
    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("no mentions here", []);
  });
});
```

If the mocked member shape does not satisfy the real `Member` type, widen the
mock to whatever `workspaceApi.listMembers` returns — read
`web/src/features/workspace/workspace.api.ts` and copy that type's fields.

- [ ] **Step 2: Run it to verify it fails**

Run: `cd web && pnpm test -- CommentComposer`
Expected: FAIL — no `mention-item` test id exists, so `findByTestId` times out.

- [ ] **Step 3: Report the picked user from the hook**

In `web/src/features/comments/MentionPicker.tsx`, widen the hook signature and
`pick`:

```tsx
export function useMentionPicker(
  value: string,
  onChange: (v: string) => void,
  onPickUser?: (userId: string) => void,
) {
```

```tsx
  function pick(member: { user_id: string; display_name: string }) {
    if (!mention) return;
    const before = value.slice(0, mention.atOffset);
    const after = value.slice(cursor);
    onChange(before + `@${member.display_name} ` + after);
    onPickUser?.(member.user_id);
    setHighlightIndex(0);
  }
```

Update both callers of `pick` — the `Enter` branch in `onKeyDown` becomes
`pick(picked)`, and the row's `onMouseDown` becomes `pick(m)`. Add the test id
to each row:

```tsx
          data-testid="mention-item"
```

- [ ] **Step 4: Accumulate ids in the composer**

In `CommentComposer.tsx`, widen the prop and hold the ids:

```tsx
  onSubmit: (body: string, mentions: string[]) => void;
```

```tsx
  const [body, setBody] = useState("");
  const [mentions, setMentions] = useState<string[]>([]);
  const { textareaProps, picker } = useMentionPicker(body, setBody, (id) =>
    setMentions((prev) => (prev.includes(id) ? prev : [...prev, id])),
  );

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const trimmed = body.trim();
    if (!trimmed) return;
    onSubmit(trimmed, mentions);
    setBody("");
    setMentions([]);
  }
```

- [ ] **Step 5: Send them**

In `web/src/lib/comments.api.ts`, add a trailing `mentions: string[] = []`
parameter to `createThread` and `createReply`, and include `mentions` in each
request body object.

In `CommentSidebar.tsx`, change the mutation variables from `string` to
`{ body: string; mentions: string[] }`:

```tsx
  const createThread = useMutation({
    mutationFn: (v: { body: string; mentions: string[] }) =>
      commentsApi.createThread(
        docId,
        v.body,
        pendingAnchor?.positionY ?? null,
        pendingAnchor?.positionYEnd ?? null,
        pendingAnchor?.anchorText ?? null,
        v.mentions,
      ),
```

and the composer callback to `onSubmit={(body, mentions) => createThread.mutate({ body, mentions })}`.
Apply the same change to the reply mutation and its composer in the same file.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd web && pnpm test`
Expected: PASS — the whole web suite, including both new cases.

- [ ] **Step 7: Typecheck, lint, commit**

```bash
cd web && pnpm tsc --noEmit && pnpm lint && cd ..
git add web/src
git commit -m "feat(web): send resolved user ids with comment mentions"
```

---

### Task 12: Thread deep-linking

Smaller than it looks: `web/src/stores/ui.ts` already exposes
`activeCommentId` / `setActiveCommentId`, and `CommentThread.tsx:201-219`
already scrolls the active thread into view and focus-rings it. A thread's id
*is* its root comment's id, which is what `activeCommentId` holds. All that is
missing is reading the URL.

**Files:**

- Modify: `web/src/features/docs/DocPage.tsx` (read `?thread=`, open the sidebar, set the active thread)

**Interfaces:**

- Consumes: `useSearchParams` (react-router-dom), `useUi` selectors `openCommentSidebar` and `setActiveCommentId`.
- Produces: `/doc/:id?thread=<uuid>` opens the comment sidebar with that thread scrolled into view.
- [ ] **Step 1: Read the current wiring**

```bash
grep -n "useUi\|useSearchParams\|commentSidebarOpen" web/src/features/docs/DocPage.tsx
sed -n '195,225p' web/src/features/comments/CommentThread.tsx
```

Confirm `CommentThread` compares `activeCommentId === threadId` and calls
`scrollIntoView` — that is the behaviour this task reuses rather than rebuilds.

- [ ] **Step 2: Open the thread from the query parameter**

In `DocPage.tsx`, add to the imports:

```tsx
import { useSearchParams } from "react-router-dom";
```

and inside the component:

```tsx
  const [searchParams] = useSearchParams();
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);

  // Arriving from a notification: /doc/:id?thread=<uuid>. The sidebar's
  // existing active-thread machinery does the scrolling and the focus ring.
  const threadParam = searchParams.get("thread");
  useEffect(() => {
    if (!threadParam) return;
    openCommentSidebar();
    setActiveCommentId(threadParam);
  }, [threadParam, openCommentSidebar, setActiveCommentId]);
```

If `useUi` is already imported with different selector bindings in this file,
reuse those rather than adding duplicates.

- [ ] **Step 3: Verify by hand**

```bash
make compose.up && make dev
```

Open a document, post a comment, read its `thread_id` from
`GET /api/docs/<id>/comments`, then visit `/doc/<id>?thread=<thread_id>`. The
sidebar must open with that thread scrolled into view and ringed.

- [ ] **Step 4: Typecheck, lint, commit**

```bash
cd web && pnpm tsc --noEmit && pnpm lint && cd ..
git add web/src
git commit -m "feat(web): deep-link a comment thread from a notification"
```

---

### Task 13: End-to-end proof, and removing the dead plumbing

**Files:**

- Create: `e2e/flows/notifications.spec.ts`
- Modify: `web/src/features/editor/KnotProvider.ts` (delete `MSG_MENTION`, `MentionMsg`, the `mention` listener list and its dispatch)
- Modify: `crates/knot-server/src/protocol.rs` if it declares a mention constant (it does not today — verify with `grep -n "MSG_MENTION" crates/knot-server/src/protocol.rs` and leave it alone if empty)

**Interfaces:**

- Consumes: everything above.
- Produces: no new interfaces; removes `KnotProvider.on("mention", …)`.
- [ ] **Step 1: Write the e2e spec**

Create `e2e/flows/notifications.spec.ts`. Every test id below already exists in
the app except the ones this plan added (`sidebar-inbox`, `inbox-badge`,
`notifications-dropdown`, `notification-row`, `notifications-page`,
`mention-item` on the comment picker) — cross-checked against
`e2e/flows/comments.spec.ts` and `e2e/flows/invite-password.spec.ts`.

```ts
import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

/**
 * Acceptance proof for the inbox:
 *
 *   owner comments, mentioning an editor picked from the mention picker
 *     → a notifications row is written for that editor
 *     → their badge shows 1
 *     → the dropdown lists it; clicking lands on the doc with the thread open
 *     → the badge clears
 *
 * The mentioned member's display name contains a space, which the server's
 * @(\w+) fallback can never resolve — so this also proves the picker is
 * sending user ids.
 */
test("a mention reaches the mentioned user's inbox", async ({ browser }) => {
  test.setTimeout(120_000);

  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();

  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("owner@inbox.test");
  await owner.getByTestId("setup-display-name").fill("Owner");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);

  // An editor whose display name contains a space.
  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("mara@inbox.test");
  await owner.getByTestId("invite-display-name").fill("Mara Jade");
  await owner.getByTestId("invite-role").selectOption("editor");
  await owner.getByTestId("invite-password").fill("mara-hunter22");
  await owner.getByTestId("invite-submit").click();

  await owner.goto("/");
  await owner.getByTestId("new-doc").click();
  await owner.getByTestId("new-doc-blank").click();
  await owner.waitForURL(/\/doc\/.+/);
  const docId = owner.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
  expect(docId).toBeTruthy();
  await expect(owner.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 30_000,
  });

  // Comment mentioning Mara, picked from the picker so her id is sent.
  await owner.getByTestId("open-comments").click();
  await expect(owner.getByTestId("comment-sidebar")).toBeVisible();
  const input = owner.getByTestId("comment-composer-input-new");
  await input.click();
  // pressSequentially fires real key events; the picker opens on keyup.
  await input.pressSequentially("please review @Mara");
  await owner.getByTestId("mention-item").first().click();
  await owner.getByTestId("comment-composer-submit-new").click();

  // Mara logs in and finds it waiting.
  const maraCtx = await browser.newContext();
  const mara = await maraCtx.newPage();
  await mara.goto("/login");
  await mara.getByTestId("login-email").fill("mara@inbox.test");
  await mara.getByTestId("login-password").fill("mara-hunter22");
  await mara.getByTestId("login-submit").click();
  await mara.waitForURL(/\/(?:doc\/.+)?$/);

  await expect(mara.getByTestId("inbox-badge")).toHaveText("1");

  await mara.getByTestId("sidebar-inbox").click();
  await expect(mara.getByTestId("notifications-dropdown")).toBeVisible();
  const row = mara.getByTestId("notification-row").first();
  await expect(row).toHaveAttribute("data-kind", "mention");
  await row.click();

  await mara.waitForURL(new RegExp(`/doc/${docId}\\?thread=`));
  await expect(mara.getByTestId("comment-sidebar")).toBeVisible();
  await expect(mara.getByTestId("inbox-badge")).toHaveCount(0);

  // The full page renders too.
  await mara.goto("/notifications");
  await expect(mara.getByTestId("notifications-page")).toBeVisible();
  await expect(mara.getByTestId("notification-row").first()).toBeVisible();

  await ownerCtx.close();
  await maraCtx.close();
});
```

- [ ] **Step 2: Run it**

```bash
make compose.up
cd e2e && pnpm playwright test notifications.spec.ts
```

Expected: PASS. If the badge assertion is flaky, the poll has not fired — add `await mara.reload()` before it rather than lengthening the timeout, since window focus and reload are the two documented refresh paths.

- [ ] **Step 3: Delete the dead mention plumbing**

In `web/src/features/editor/KnotProvider.ts`, remove:

- the `MSG_MENTION` constant and the header comment describing it
- the `MentionMsg` type
- `mention` from the `Listeners` type and from the `listeners` initialiser
- the `if (msg.type === "mention") { … }` branch in the message handler

Then check nothing referenced it:

```bash
grep -rn "MSG_MENTION\|MentionMsg\|\"mention\"" web/src/features/editor
```

Expected: no matches outside `MentionExtension.ts` (which is the editor's own mention UI and stays).

- [ ] **Step 4: Full gate**

```bash
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cd web && pnpm tsc --noEmit && pnpm lint && pnpm test && cd ..
cd e2e && pnpm playwright test && cd ..
```

Expected: everything green.

- [ ] **Step 5: Commit**

```bash
git add e2e web/src
git commit -m "test(e2e): prove a mention reaches the inbox

Also deletes the MSG_MENTION plumbing reserved in June: delivery is
per-user now, so the doc WebSocket is not on the path."
```

---

### Task 14: Changelog

**Files:**

- Modify: `CHANGELOG.md:9` (the `## [Unreleased]` section)
- [ ] **Step 1: Write the entry**

Under `## [Unreleased]`, add:

```markdown
### Added
- **Notifications and an inbox.** An `@mention` in a comment used to fire a
  `pg_notify('comment_mentions', …)` that no process in the codebase ever
  listened for, into a `MSG_MENTION` frame the frontend reserved and never
  received. Mentioning a colleague did nothing they would ever see. Five events
  now write to a `notifications` table — mentions, replies in threads you are
  part of, task assignment, overdue tasks, and documents shared with you — read
  through an Inbox in the sidebar with an unread badge and a `/notifications`
  page. Delivery is a 30-second poll that also refreshes on window focus; the
  table is shaped as an outbox (`emailed_at`) so email can arrive later without
  a migration. Idempotency is a unique index on `(user_id, dedupe_key)` rather
  than application logic, which is what lets the overdue sweep run on every
  replica with no leader election.

### Fixed
- **Members whose display name contains a space could not be mentioned.**
  Comment mentions were resolved server-side by matching `@(\w+)` against
  member display names, so `@Christian Hüning` captured `Christian`, matched
  nobody, and silently notified no one. The mention picker now sends the user
  ids it resolved; the regex remains as a fallback for comments written before
  this release.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog entry for notifications and the inbox"
```

---

## Verification

After Task 14, the whole feature is proven by:

```bash
make compose.up
make test          # cargo nextest + vitest
make lint          # clippy + fmt + tsc
make e2e           # playwright, including notifications.spec.ts
```

Every one must pass before the branch is offered for review.
