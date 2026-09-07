//! Integration tests for `PgTaskStore`. Uses `knot_test_support::fresh_db`
//! against the dev compose Postgres — never spawns a new container.

use knot_storage::{
    DocStore, DocTaskInput, PgDocStore, PgTaskStore, PgUserStore, PgWorkspaceStore, TaskStore,
    UserStore, WorkspaceRole, WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

async fn setup() -> (PgTaskStore, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let u = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let sk = sort_key_between(None, None);
    let doc = docs.create(ws.id, None, "Doc", &sk, u.id).await.unwrap();
    (PgTaskStore::new(pool), ws.id, doc.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_then_list_returns_rows() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    let items = vec![
        DocTaskInput {
            item_index: 0,
            text: "Buy milk".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
        DocTaskInput {
            item_index: 1,
            text: "Read book".into(),
            assignee_user_id: Some(user_id),
            checked: true,
            due_at: None,
        },
    ];
    store
        .upsert_for_doc(ws_id, doc_id, &items, None)
        .await
        .unwrap();
    let in_doc = store.list_for_doc(doc_id).await.unwrap();
    assert_eq!(in_doc.len(), 2);
    let mine = store.list_for_assignee(ws_id, user_id, true).await.unwrap();
    assert_eq!(mine.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_excludes_completed_by_default() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    let items = vec![
        DocTaskInput {
            item_index: 0,
            text: "Open".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
        DocTaskInput {
            item_index: 1,
            text: "Done".into(),
            assignee_user_id: Some(user_id),
            checked: true,
            due_at: None,
        },
    ];
    store
        .upsert_for_doc(ws_id, doc_id, &items, None)
        .await
        .unwrap();
    let open_only = store
        .list_for_assignee(ws_id, user_id, false)
        .await
        .unwrap();
    assert_eq!(open_only.len(), 1);
    assert_eq!(open_only[0].text, "Open");
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_replaces_set_dropping_removed_items() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    // First pass: three items.
    let v1 = vec![
        DocTaskInput {
            item_index: 0,
            text: "A".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
        DocTaskInput {
            item_index: 1,
            text: "B".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
        DocTaskInput {
            item_index: 2,
            text: "C".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
    ];
    store
        .upsert_for_doc(ws_id, doc_id, &v1, None)
        .await
        .unwrap();
    assert_eq!(store.list_for_doc(doc_id).await.unwrap().len(), 3);
    // Second pass: only index 0 and 2 remain. Index 1 must be deleted.
    let v2 = vec![
        DocTaskInput {
            item_index: 0,
            text: "A".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
        DocTaskInput {
            item_index: 2,
            text: "C-renamed".into(),
            assignee_user_id: Some(user_id),
            checked: false,
            due_at: None,
        },
    ];
    store
        .upsert_for_doc(ws_id, doc_id, &v2, None)
        .await
        .unwrap();
    let after = store.list_for_doc(doc_id).await.unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|t| t.item_index == 0));
    assert!(after.iter().any(|t| t.item_index == 2));
    assert!(after.iter().all(|t| t.item_index != 1));
    // C's text reflects the rename.
    let c = after.iter().find(|t| t.item_index == 2).unwrap();
    assert_eq!(c.text, "C-renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_upsert_clears_all_doc_tasks() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    let v1 = vec![DocTaskInput {
        item_index: 0,
        text: "only".into(),
        assignee_user_id: Some(user_id),
        checked: false,
        due_at: None,
    }];
    store
        .upsert_for_doc(ws_id, doc_id, &v1, None)
        .await
        .unwrap();
    assert_eq!(store.list_for_doc(doc_id).await.unwrap().len(), 1);
    store
        .upsert_for_doc(ws_id, doc_id, &[], None)
        .await
        .unwrap();
    assert_eq!(store.list_for_doc(doc_id).await.unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn checked_transition_stamps_completed_at_and_clears_on_uncheck() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    // Start unchecked.
    store
        .upsert_for_doc(
            ws_id,
            doc_id,
            &[DocTaskInput {
                item_index: 0,
                text: "t".into(),
                assignee_user_id: Some(user_id),
                checked: false,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();
    assert!(
        store.list_for_doc(doc_id).await.unwrap()[0]
            .completed_at
            .is_none()
    );
    // Flip to checked: completed_at populated.
    store
        .upsert_for_doc(
            ws_id,
            doc_id,
            &[DocTaskInput {
                item_index: 0,
                text: "t".into(),
                assignee_user_id: Some(user_id),
                checked: true,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();
    assert!(
        store.list_for_doc(doc_id).await.unwrap()[0]
            .completed_at
            .is_some()
    );
    // Flip back: completed_at cleared.
    store
        .upsert_for_doc(
            ws_id,
            doc_id,
            &[DocTaskInput {
                item_index: 0,
                text: "t".into(),
                assignee_user_id: Some(user_id),
                checked: false,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();
    assert!(
        store.list_for_doc(doc_id).await.unwrap()[0]
            .completed_at
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_checked_preserves_completed_at_across_reindex() {
    let (store, ws_id, doc_id, user_id) = setup().await;
    // Land as checked once → completed_at populated.
    store
        .upsert_for_doc(
            ws_id,
            doc_id,
            &[DocTaskInput {
                item_index: 0,
                text: "t".into(),
                assignee_user_id: Some(user_id),
                checked: true,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();
    let first = store.list_for_doc(doc_id).await.unwrap()[0]
        .completed_at
        .expect("expected completed_at after first check");
    // Re-upsert identical checked=true content; completed_at must not move.
    store
        .upsert_for_doc(
            ws_id,
            doc_id,
            &[DocTaskInput {
                item_index: 0,
                text: "t".into(),
                assignee_user_id: Some(user_id),
                checked: true,
                due_at: None,
            }],
            None,
        )
        .await
        .unwrap();
    let second = store.list_for_doc(doc_id).await.unwrap()[0]
        .completed_at
        .expect("expected completed_at preserved across reindex");
    assert_eq!(first, second);
}
