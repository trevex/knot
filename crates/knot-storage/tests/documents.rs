use knot_storage::{
    DocStore, DocStoreError, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole,
    WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

async fn setup() -> (PgDocStore, Uuid, Uuid) {
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
    (PgDocStore::new(pool), ws.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_get_list_lifecycle() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Hello", &sk, user).await.unwrap();
    assert_eq!(doc.title, "Hello");
    assert_eq!(doc.workspace_id, ws);
    let got = store.get(doc.id).await.unwrap().unwrap();
    assert_eq!(got.id, doc.id);
    let list = store.list_alive(ws).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_updates_title_and_icon() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Old", &sk, user).await.unwrap();
    let new = store
        .rename(ws, doc.id, user, "New", Some("📄"))
        .await
        .unwrap();
    assert_eq!(new.title, "New");
    assert_eq!(new.icon.as_deref(), Some("📄"));
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_hides_and_restore_brings_back() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "X", &sk, user).await.unwrap();
    store.archive(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 0);
    store.restore(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_under_new_parent() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, None, "B", "n", user).await.unwrap();
    let moved = store
        .move_to(ws, b.id, user, Some(a.id), "m")
        .await
        .unwrap();
    assert_eq!(moved.parent_id, Some(a.id));
    let kids = store.siblings(ws, Some(a.id)).await.unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, b.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_not_found() {
    let (store, ws, user) = setup().await;
    let err = store
        .rename(ws, Uuid::new_v4(), user, "X", None)
        .await
        .unwrap_err();
    assert!(matches!(err, knot_storage::DocStoreError::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn descendant_ids_returns_full_subtree() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", user).await.unwrap();
    let c = store.create(ws, Some(b.id), "C", "m", user).await.unwrap();
    let _d = store.create(ws, None, "D", "n", user).await.unwrap(); // unrelated root

    let descendants = store.descendant_ids(a.id).await.unwrap();
    assert_eq!(descendants.len(), 2);
    assert!(descendants.contains(&b.id));
    assert!(descendants.contains(&c.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn templates_flow_set_and_list() {
    let (store, ws, user) = setup().await;
    // Two regular docs + one template.
    let sk = sort_key_between(None, None);
    let a = store.create(ws, None, "A", &sk, user).await.unwrap();
    assert!(!a.is_template);
    let sk = sort_key_between(None, None);
    let b = store.create(ws, None, "B", &sk, user).await.unwrap();
    let sk = sort_key_between(None, None);
    let tpl = store
        .create(ws, None, "Meeting notes", &sk, user)
        .await
        .unwrap();
    // Flip the template flag.
    let after = store.set_template(ws, tpl.id, user, true).await.unwrap();
    assert!(after.is_template);
    // list_alive must exclude the template.
    let alive = store.list_alive(ws).await.unwrap();
    let ids: Vec<_> = alive.iter().map(|d| d.id).collect();
    assert!(ids.contains(&a.id));
    assert!(ids.contains(&b.id));
    assert!(!ids.contains(&tpl.id));
    // list_templates returns just the template.
    let templates = store.list_templates(ws).await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].id, tpl.id);
    // Unmark restores it to the main tree.
    store.set_template(ws, tpl.id, user, false).await.unwrap();
    let alive2 = store.list_alive(ws).await.unwrap();
    assert!(alive2.iter().any(|d| d.id == tpl.id));
    let templates2 = store.list_templates(ws).await.unwrap();
    assert!(templates2.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_descendant_is_rejected_as_cycle() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "m", u).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", u).await.unwrap();
    let err = store
        .move_to(ws, a.id, u, Some(b.id), "n")
        .await
        .unwrap_err();
    assert!(
        matches!(err, DocStoreError::Cycle),
        "expected Cycle, got {err:?}"
    );
    let err = store
        .move_to(ws, a.id, u, Some(a.id), "n")
        .await
        .unwrap_err();
    assert!(matches!(err, DocStoreError::Cycle));
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_reorder_and_nest_preserve_all_docs() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "a", u).await.unwrap();
    let b = store.create(ws, None, "B", "b", u).await.unwrap();
    let c = store.create(ws, None, "C", "c", u).await.unwrap();
    store.move_to(ws, c.id, u, Some(a.id), "m").await.unwrap();
    store.move_to(ws, b.id, u, None, "z").await.unwrap();
    let all = store.list_alive(ws).await.unwrap();
    assert_eq!(all.len(), 3, "no doc lost");
    assert_eq!(
        all.iter().find(|d| d.id == c.id).unwrap().parent_id,
        Some(a.id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn heal_query_promotes_cycle_members_to_root() {
    let pool = knot_test_support::fresh_db().await.pool;
    let w = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "U", "$h$")
        .await
        .unwrap();
    let store = PgDocStore::new(pool.clone());
    let a = store.create(w.id, None, "A", "a", u.id).await.unwrap();
    let b = store
        .create(w.id, Some(a.id), "B", "b", u.id)
        .await
        .unwrap();

    // Inject a cycle directly (bypassing the move guard): A.parent = B.
    sqlx::query("UPDATE documents SET parent_id = $1 WHERE id = $2")
        .bind(b.id)
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();

    // Run the heal statement (identical SQL to the migration).
    sqlx::query(
        "WITH RECURSIVE anc(start, cur, depth) AS (
             SELECT id, parent_id, 1 FROM documents WHERE parent_id IS NOT NULL
             UNION ALL
             SELECT a.start, d.parent_id, a.depth + 1
             FROM anc a JOIN documents d ON d.id = a.cur
             WHERE a.cur IS NOT NULL AND a.depth < 1000
         )
         UPDATE documents SET parent_id = NULL, updated_at = now()
         WHERE id IN (SELECT start FROM anc WHERE cur = start)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let all = store.list_alive(w.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.parent_id.is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn siblings_include_archived_so_sort_keys_are_not_reused() {
    let (store, ws, user) = setup().await;
    let parent = store.create(ws, None, "P", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(parent.id), "C", "m", user)
        .await
        .unwrap();
    store.archive(ws, child.id, user).await.unwrap();

    // The archived child keeps its sort_key and still counts towards
    // UNIQUE (workspace_id, parent_id, sort_key). If generation cannot see
    // it, the next subpage regenerates "m" and the INSERT conflicts.
    let sibs = store.siblings(ws, Some(parent.id)).await.unwrap();
    assert_eq!(sibs.len(), 1, "archived siblings must stay visible here");

    let next = sort_key_between(None, sibs.first().map(|d| d.sort_key.as_str()));
    assert_ne!(next, child.sort_key);
    store
        .create(ws, Some(parent.id), "C2", &next, user)
        .await
        .unwrap();
}
