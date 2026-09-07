//! Integration: comment writes produce inbox rows for the right people.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
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
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let alice = s
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    let bob = s
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Test Doc", "m", alice.id)
        .await
        .unwrap();
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
    assert_eq!(
        res.status(),
        StatusCode::NO_CONTENT,
        "login failed for {email}"
    );
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

/// Same as `post_json` but for the `PATCH` comment-edit endpoint.
async fn patch_json(
    state: &AppState,
    cookie: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("PATCH")
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
    assert!(
        notifications
            .list(alice, false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
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
    assert!(
        notifications
            .list(bob, false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
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

    let rows = state
        .notifications
        .as_ref()
        .unwrap()
        .list(alice, false, 50, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "mention wins; no duplicate reply row");
    assert_eq!(rows[0].kind, "mention");
    let _ = bob;
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_a_comment_does_not_notify_thread_participants() {
    let (state, _ws, doc, alice, bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    // Alice opens a thread.
    let (_, thread) = post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "original text" }),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap();
    let comment_id = thread["id"].as_str().unwrap().to_string();

    // Bob replies, becoming a thread participant. This is the one
    // legitimate reply notification in this test, and it goes to Alice —
    // Bob himself gets nothing from his own reply.
    post_json(
        &state,
        &bob_cookie,
        &format!("/api/docs/{doc}/comments/{thread_id}/replies"),
        serde_json::json!({ "body": "looks fine" }),
    )
    .await;

    let notifications = state.notifications.as_ref().unwrap();
    assert_eq!(
        notifications
            .list(alice, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "Alice should have exactly the one reply notification, from Bob's reply"
    );

    // Alice edits her original comment — a typo fix, no new mention. This
    // must NOT tell Bob "Alice replied": nobody replied, she edited.
    let (status, _) = patch_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments/{comment_id}"),
        serde_json::json!({ "body": "original text, fixed" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let bobs = notifications.list(bob, false, 50, None).await.unwrap();
    assert!(
        bobs.is_empty(),
        "editing a comment must not fan out reply notifications to thread participants; got {bobs:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_a_comment_to_add_a_mention_notifies_exactly_once() {
    let (state, _ws, doc, alice, bob) = seeded().await;
    let _ = alice;
    let alice_cookie = login(&state, "alice@example.com").await;

    // Alice opens a thread with no mention — solo thread, so no reply
    // fan-out is in play either; this isolates the mention-on-edit path.
    let (_, thread) = post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "no mentions here" }),
    )
    .await;
    let comment_id = thread["id"].as_str().unwrap().to_string();

    let notifications = state.notifications.as_ref().unwrap();
    assert!(
        notifications
            .list(bob, false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );

    // Edit adds a mention: the newly-mentioned Bob is notified exactly once.
    let (status, _) = patch_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments/{comment_id}"),
        serde_json::json!({ "body": "please look @Bob" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let bobs = notifications.list(bob, false, 50, None).await.unwrap();
    assert_eq!(bobs.len(), 1);
    assert_eq!(bobs[0].kind, "mention");

    // Editing again while keeping the same mention must not add a second
    // row for Bob — he is already mentioned on this comment.
    let (status, _) = patch_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments/{comment_id}"),
        serde_json::json!({ "body": "please look @Bob, thanks" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let bobs = notifications.list(bob, false, 50, None).await.unwrap();
    assert_eq!(
        bobs.len(),
        1,
        "no duplicate mention row when a second edit keeps the same mention"
    );
}

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

    let rows = state
        .notifications
        .as_ref()
        .unwrap()
        .list(bob, false, 50, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "doc_shared");
    assert_eq!(rows[0].doc_id, Some(doc));
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_mention_ids_reach_a_user_whose_name_has_a_space() {
    let (state, ws, doc, _alice, _bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    // A member the display-name regex can never match.
    let hash = state.hasher.hash("hunter22").unwrap();
    let carol = state
        .users
        .as_ref()
        .unwrap()
        .create_local("carol@example.com", "Carol Danvers", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws, carol.id, WorkspaceRole::Editor)
        .await
        .unwrap();

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

    let rows = state
        .notifications
        .as_ref()
        .unwrap()
        .list(carol.id, false, 50, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "mention");
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_and_typed_mentions_union_instead_of_either_or() {
    // Finding 6: picking one person from the mention picker then
    // hand-typing a second `@name` (e.g. after a trailing space silently
    // closed the picker) used to drop the typed name entirely, because a
    // non-empty `mentions` list disabled the regex fallback outright.
    let (state, ws, doc, _alice, bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    // A member the display-name regex can never match on its own — proves
    // the explicit id still works when unioned, not just when alone.
    let hash = state.hasher.hash("hunter22").unwrap();
    let carol = state
        .users
        .as_ref()
        .unwrap()
        .create_local("carol@example.com", "Carol Danvers", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws, carol.id, WorkspaceRole::Editor)
        .await
        .unwrap();

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({
            "body": "over to you @Carol Danvers, and also @Bob",
            "mentions": [carol.id.to_string()],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let notifications = state.notifications.as_ref().unwrap();
    assert_eq!(
        notifications
            .list(carol.id, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "the picked id must still notify"
    );
    assert_eq!(
        notifications
            .list(bob, false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "the hand-typed name must not be dropped just because `mentions` was non-empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_id_for_a_non_member_is_ignored() {
    let (state, _ws, doc, _alice, _bob) = seeded().await;
    let cookie = login(&state, "alice@example.com").await;

    // A real user who is not a member of this workspace.
    let hash = state.hasher.hash("hunter22").unwrap();
    let outsider = state
        .users
        .as_ref()
        .unwrap()
        .create_local("mallory@example.com", "Mallory", &hash)
        .await
        .unwrap();

    let (status, _) = post_json(
        &state,
        &cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hi", "mentions": [outsider.id.to_string()] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    assert!(
        state
            .notifications
            .as_ref()
            .unwrap()
            .list(outsider.id, false, 50, None)
            .await
            .unwrap()
            .is_empty(),
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
        state
            .notifications
            .as_ref()
            .unwrap()
            .list(bob, false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

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
    let other_ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("other", "Other")
        .await
        .unwrap();
    let other_doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(other_ws.id, None, "Elsewhere", "m", bob)
        .await
        .unwrap();
    state
        .notifications
        .as_ref()
        .unwrap()
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
        state
            .notifications
            .as_ref()
            .unwrap()
            .list(bob, false, 50, None)
            .await
            .unwrap()
            .len(),
        2
    );
    let items = get_json(&state, &bob_cookie, "/api/notifications").await.1;
    let items = items["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the unreadable row is filtered, not 403'd");
    assert_eq!(items[0]["kind"], "mention");
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_read_with_all_true_clears_the_whole_inbox() {
    // `POST /api/notifications/read {"all": true}` had no coverage at any
    // level. `ids` defaults to `[]` and `all` to `false`, so a body that
    // fails to deserialize as expected (e.g. a wrong key name) silently
    // degrades to "mark nothing" and still returns 204 — the "Mark all
    // read" button would appear to work and do nothing.
    let (state, _ws, doc, _alice, bob) = seeded().await;
    let alice_cookie = login(&state, "alice@example.com").await;
    let bob_cookie = login(&state, "bob@example.com").await;

    // Two distinct unread rows for Bob: a mention, and a direct grant.
    post_json(
        &state,
        &alice_cookie,
        &format!("/api/docs/{doc}/comments"),
        serde_json::json!({ "body": "hey @Bob" }),
    )
    .await;
    let app = router_with_state(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc}/grants/user:{bob}"))
                .header("cookie", &alice_cookie)
                .header("x-csrf-token", csrf_from(&alice_cookie))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "editor", "inherit": true }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let (_, count) = get_json(&state, &bob_cookie, "/api/notifications/unread_count").await;
    assert_eq!(count["count"], 2, "sanity: two unread rows before marking");

    let (status, _) = post_json(
        &state,
        &bob_cookie,
        "/api/notifications/read",
        serde_json::json!({ "all": true }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, count) = get_json(&state, &bob_cookie, "/api/notifications/unread_count").await;
    assert_eq!(count["count"], 0, "all: true must clear every unread row");

    let (_, list) = get_json(&state, &bob_cookie, "/api/notifications").await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        items.iter().all(|i| i["read"] == true),
        "every row must now be marked read: {items:?}"
    );
}
