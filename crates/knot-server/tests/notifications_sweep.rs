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
