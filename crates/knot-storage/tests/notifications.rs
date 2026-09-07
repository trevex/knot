//! Integration tests for `PgNotificationStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use knot_storage::{
    DocStore, DocTaskInput, NewNotification, NotificationKind, NotificationStore, PgDocStore,
    PgNotificationStore, PgTaskStore, PgUserStore, PgWorkspaceStore, TaskStore, UserStore,
    WorkspaceRole, WorkspaceStore, sort_key_between, task_assigned_dedupe_key,
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
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob Smith", "$h$")
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
    let docs = PgDocStore::new(pool.clone());
    let sk = sort_key_between(None, None);
    let doc = docs
        .create(ws.id, None, "Doc", &sk, alice.id)
        .await
        .unwrap();
    (
        PgNotificationStore::new(pool),
        ws.id,
        doc.id,
        alice.id,
        bob.id,
    )
}

/// Backdate a row and optionally mark it read, so `prune` can be exercised
/// without waiting 90 days. Issued directly against the pool rather than
/// through a test-only method on `PgNotificationStore`, since that store is
/// production API surface.
async fn backdate(store: &PgNotificationStore, id: i64, days_old: i64, read: bool) {
    sqlx::query(
        "UPDATE notifications \
         SET created_at = now() - ($2 || ' days')::interval, \
             read_at = CASE WHEN $3 THEN now() - ($2 || ' days')::interval ELSE NULL END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(days_old.to_string())
    .bind(read)
    .execute(store.pool())
    .await
    .unwrap();
}

/// Hard-delete a document to prove the FK cascade, issued directly against
/// the pool rather than through a test-only method on `PgNotificationStore`.
async fn hard_delete_doc(store: &PgNotificationStore, doc_id: Uuid) {
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc_id)
        .execute(store.pool())
        .await
        .unwrap();
}

fn mention_for(
    ws: Uuid,
    doc: Uuid,
    recipient: Uuid,
    actor: Uuid,
    comment: Uuid,
) -> NewNotification {
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
    assert!(
        store
            .emit(&mention_for(ws, doc, bob, alice, comment))
            .await
            .unwrap()
    );

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
    assert!(
        !store
            .emit(&mention_for(ws, doc, alice, alice, comment))
            .await
            .unwrap()
    );
    assert!(store.list(alice, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_count_caps_and_mark_read_clears() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
    }
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 3);
    // The cap actually binds: with 3 unread rows, a cap below that count
    // must clip the result rather than merely bounding it from above.
    assert_eq!(store.unread_count(bob, 2).await.unwrap(), 2);

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
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    let bobs = store.list(bob, true, 50, None).await.unwrap();

    assert_eq!(store.mark_read(alice, &[bobs[0].id]).await.unwrap(), 0);
    assert_eq!(store.unread_count(bob, 100).await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_pages_backwards_on_the_cursor() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
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
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
    }
    let rows = store.list(bob, false, 50, None).await.unwrap();
    // Row 0: read, 100 days old  -> pruned.
    // Row 1: unread, 100 days old -> kept.
    // Row 2: unread, 200 days old -> pruned.
    backdate(&store, rows[0].id, 100, true).await;
    backdate(&store, rows[1].id, 100, false).await;
    backdate(&store, rows[2].id, 200, false).await;

    assert_eq!(store.prune(chrono::Utc::now()).await.unwrap(), 2);
    let left = store.list(bob, false, 50, None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, rows[1].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn prune_never_touches_task_assigned_even_when_read_and_old() {
    // task_assigned is the one kind whose persisted row is the *only*
    // thing suppressing a re-notification: PgTaskStore::upsert_for_doc
    // re-derives it from doc_tasks on every reindex and relies on the
    // dedupe row already existing. If retention pruned it like every other
    // kind, the next edit of a document — any edit, unrelated to the task
    // — would re-notify the assignee about a months-old, unchanged
    // assignment. A same-age `mention` row is the control: it must still
    // go, proving this isn't just a floor that swallowed everything.
    let (store, ws, doc, alice, bob) = setup().await;
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    store
        .emit(&NewNotification {
            workspace_id: ws,
            user_id: bob,
            actor_id: None,
            kind: NotificationKind::TaskAssigned,
            doc_id: Some(doc),
            target_kind: "task".into(),
            target_id: format!("{doc}:0"),
            dedupe_key: format!("task_assigned:{doc}:deadbeefcafebabe:{bob}"),
            data: serde_json::json!({ "excerpt": "old task" }),
        })
        .await
        .unwrap();

    let rows = store.list(bob, false, 50, None).await.unwrap();
    let task_assigned = rows.iter().find(|r| r.kind == "task_assigned").unwrap().id;
    let mention = rows.iter().find(|r| r.kind == "mention").unwrap().id;
    // Both read, both well past the 90-day read-row threshold.
    backdate(&store, task_assigned, 200, true).await;
    backdate(&store, mention, 200, true).await;

    assert_eq!(
        store.prune(chrono::Utc::now()).await.unwrap(),
        1,
        "only the mention row should be deleted"
    );
    let left = store.list(bob, false, 50, None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].id, task_assigned,
        "the read, 200-day-old task_assigned row must survive prune"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_doc_cascades_its_notifications() {
    let (store, ws, doc, alice, bob) = setup().await;
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    hard_delete_doc(&store, doc).await;
    assert!(store.list(bob, false, 50, None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_notifies_once_and_survives_a_reorder() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let carol = users
        .create_local("carol@x.test", "Carol", "$h$")
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
    ws_store
        .add_member(ws.id, carol.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let doc = docs
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    let tasks = PgTaskStore::new(pool.clone());
    let notifications = PgNotificationStore::new(pool.clone());

    // Different assignees per item on purpose: after the swap below, each
    // task id ("<doc_id>:<item_index>") is paired with a (text, assignee)
    // combination it has never held before. If both items shared one
    // assignee, an id-keyed dedupe scheme would report the same
    // notification counts as the content-keyed one and this test would
    // pass even against that regression.
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
        assignee_user_id: Some(carol.id),
        checked: false,
        due_at: None,
    };

    tasks
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[ship.clone(), review.clone()],
            Some(alice.id),
        )
        .await
        .unwrap();
    assert_eq!(
        notifications
            .list(bob.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        notifications
            .list(carol.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );

    // Swap the two items: "ship it" moves to index 1, "review it" moves to
    // index 0. Every task id changes, and — because the assignees differ —
    // each id is now paired with an assignee it never held before either
    // (id 0 was ship/bob, is now review/carol; id 1 was review/carol, is
    // now ship/bob). An id-keyed dedupe would treat both as brand-new
    // assignments and re-notify; content-keyed dedupe must not.
    let swapped = vec![
        DocTaskInput {
            item_index: 0,
            ..review.clone()
        },
        DocTaskInput {
            item_index: 1,
            ..ship.clone()
        },
    ];
    tasks
        .upsert_for_doc(ws.id, doc.id, &swapped, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(
        notifications
            .list(bob.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "reordering a list must not re-notify"
    );
    assert_eq!(
        notifications
            .list(carol.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "reordering a list must not re-notify"
    );

    // Editing the text is a new task as far as the key is concerned.
    // "ship it" now lives at item_index 1, still assigned to bob.
    let edited = vec![DocTaskInput {
        text: "ship it today".into(),
        ..swapped[1].clone()
    }];
    tasks
        .upsert_for_doc(ws.id, doc.id, &edited, Some(alice.id))
        .await
        .unwrap();
    assert_eq!(
        notifications
            .list(bob.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        notifications
            .list(carol.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_with_no_known_actor_still_notifies() {
    // Live edits arrive through the reindex worker (crates/knot-server/src/
    // reindex.rs), which only ever has a doc id, never an editor identity —
    // so `refresh_markdown_and_index` always calls `upsert_for_doc` with
    // `actor_id: None` on that path. The self-assignment guard compares
    // the assignee against `actor_id`, so with `actor_id: None` it can
    // never suppress: this pins that deliberate limitation, not a bug.
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
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
                text: "assigned to myself with no known actor".into(),
                assignee_user_id: Some(alice.id),
                checked: false,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        PgNotificationStore::new(pool)
            .list(alice.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "actor_id: None can't suppress self-assignment — that's the live-edit path"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_to_yourself_notifies_nobody() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
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
            .list(alice.id, false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_an_already_checked_task_notifies_nobody() {
    // Finding 1a: without a `checked` guard, a checklist item that was
    // completed months ago — and is only unchecked in the sense that it
    // was never notified about before this feature existed — would still
    // fire a `task_assigned` row the first time its document gets
    // reindexed. Landing a task as checked from the start (import, or any
    // path that never went through the unchecked state) must notify
    // nobody, matching a task that's simply done.
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
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
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
                text: "already done on arrival".into(),
                assignee_user_id: Some(bob.id),
                checked: true,
                due_at: None,
            }],
            Some(alice.id),
        )
        .await
        .unwrap();

    assert!(
        PgNotificationStore::new(pool)
            .list(bob.id, false, 50, None)
            .await
            .unwrap()
            .is_empty(),
        "a task that lands checked must not notify its assignee"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_and_rust_task_assigned_dedupe_keys_agree() {
    // Finding 1c's seed migration
    // (migrations/20260907120000_notifications_kind_check_and_task_assigned_seed.sql)
    // reproduces `task_assigned_dedupe_key` in raw SQL — `encode(substring(
    // sha256(convert_to(text, 'UTF8')) from 1 for 8), 'hex')` — so it can
    // pre-populate dedupe rows for tasks that already existed when this
    // feature ships. If the SQL and Rust ever disagree, the seed inserts
    // rows under keys the runtime will never look up again, and the whole
    // point of the migration (a silent upgrade) silently fails instead.
    // The non-ASCII and empty-string cases matter because sha256 operates
    // on UTF-8 bytes, not chars, and an empty task text is a valid input.
    let (store, _ws, doc, _alice, bob) = setup().await;
    for text in ["follow up", "Jörg follow up 日本語 🎉", ""] {
        let rust_key = task_assigned_dedupe_key(doc, text, bob);
        let (sql_key,): (String,) = sqlx::query_as(
            "SELECT 'task_assigned:' || $1::uuid || ':' || \
                encode(substring(sha256(convert_to($2, 'UTF8')) from 1 for 8), 'hex') || \
                ':' || $3::uuid",
        )
        .bind(doc)
        .bind(text)
        .bind(bob)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(rust_key, sql_key, "mismatch for text = {text:?}");
    }
}
