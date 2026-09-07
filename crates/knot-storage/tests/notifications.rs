//! Integration tests for `PgNotificationStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use knot_storage::{
    DocStore, NewNotification, NotificationKind, NotificationStore, PgDocStore,
    PgNotificationStore, PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore,
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
async fn deleting_a_doc_cascades_its_notifications() {
    let (store, ws, doc, alice, bob) = setup().await;
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    hard_delete_doc(&store, doc).await;
    assert!(store.list(bob, false, 50, None).await.unwrap().is_empty());
}
