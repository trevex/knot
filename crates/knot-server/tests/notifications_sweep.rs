//! The overdue-task sweep emits once per task per day and is safe to run
//! concurrently on every replica.

use chrono::{Duration, Utc};
use knot_server::notifications_sweep;
use knot_storage::{
    DocStore, DocTaskInput, NewNotification, NotificationKind, NotificationStore, PgDocStore,
    PgNotificationStore, PgTaskStore, PgUserStore, PgWorkspaceStore, TaskStore, UserStore,
    WorkspaceRole, WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn overdue_task_emits_once_per_day_even_across_concurrent_sweeps() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
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
    let before = notifications
        .list(bob.id, false, 50, None)
        .await
        .unwrap()
        .len();

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
async fn task_due_survives_a_reorder_without_re_notifying() {
    // Finding 5: `doc_tasks.id` is "<doc_id>:<item_index>". Keying
    // `task_due` on it meant inserting an item above an already-overdue
    // one shifted the id of everything below, so the dedupe index saw what
    // looked like a brand-new overdue task and fired a second `task_due`
    // the same day — the exact reorder hazard the design's §2 warns about
    // for `task_assigned`. The content-addressed key must survive this.
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    let tasks = PgTaskStore::new(pool.clone());
    let overdue = DocTaskInput {
        item_index: 0,
        text: "ship it".into(),
        assignee_user_id: Some(bob.id),
        checked: false,
        due_at: Some(Utc::now() - Duration::days(1)),
    };
    tasks
        .upsert_for_doc(
            ws.id,
            doc.id,
            std::slice::from_ref(&overdue),
            Some(alice.id),
        )
        .await
        .unwrap();

    let now = Utc::now();
    let first = notifications_sweep::run_once(&pool, now).await.unwrap();
    assert_eq!(first.due_emitted, 1, "the overdue task notifies once");

    // Insert an unrelated item above it: "ship it" moves from doc_tasks id
    // "<doc>:0" to "<doc>:1" even though nothing about the task itself —
    // text, assignee, due date — changed.
    let inserted_above = DocTaskInput {
        item_index: 0,
        text: "an unrelated new item".into(),
        assignee_user_id: None,
        checked: false,
        due_at: None,
    };
    let shifted = DocTaskInput {
        item_index: 1,
        ..overdue.clone()
    };
    tasks
        .upsert_for_doc(ws.id, doc.id, &[inserted_above, shifted], Some(alice.id))
        .await
        .unwrap();

    let second = notifications_sweep::run_once(&pool, now).await.unwrap();
    assert_eq!(
        second.due_emitted, 0,
        "a reorder that only changes doc_tasks.id must not look like a new overdue task"
    );

    let due_rows = PgNotificationStore::new(pool)
        .list(bob.id, false, 50, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "task_due")
        .count();
    assert_eq!(
        due_rows, 1,
        "bob must have exactly one task_due row across both sweeps"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checked_task_is_never_overdue() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
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

    let out = notifications_sweep::run_once(&pool, Utc::now())
        .await
        .unwrap();
    assert_eq!(out.due_emitted, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_once_prunes_via_the_notification_store_and_reports_the_count() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    let notifications = PgNotificationStore::new(pool.clone());
    notifications
        .emit(&NewNotification {
            workspace_id: ws.id,
            user_id: bob.id,
            actor_id: Some(alice.id),
            kind: NotificationKind::Mention,
            doc_id: Some(doc.id),
            target_kind: "comment".into(),
            target_id: Uuid::new_v4().to_string(),
            dedupe_key: format!("mention:{}", Uuid::new_v4()),
            data: serde_json::json!({}),
        })
        .await
        .unwrap();
    let row_id = notifications.list(bob.id, false, 50, None).await.unwrap()[0].id;

    // Backdate past the 180-day everything-goes threshold. Issued as raw
    // SQL against the pool directly — mirroring the `backdate` helper in
    // crates/knot-storage/tests/notifications.rs — rather than
    // reintroducing a test-only method on `PgNotificationStore`, which was
    // deliberately removed from that production type in Task 1.
    sqlx::query("UPDATE notifications SET created_at = now() - interval '200 days' WHERE id = $1")
        .bind(row_id)
        .execute(&pool)
        .await
        .unwrap();

    let out = notifications_sweep::run_once(&pool, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        out.pruned, 1,
        "SweepOutcome.pruned must reflect the row prune deleted"
    );
    assert!(
        notifications
            .list(bob.id, false, 50, None)
            .await
            .unwrap()
            .is_empty(),
        "the backdated row must actually be gone"
    );
}
