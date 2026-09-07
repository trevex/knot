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
