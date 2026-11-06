//! End-to-end engine test against a mock Discord API.
//!
//! A tiny axum server stands in for `discord.com/api`. It records every DELETE,
//! answers the first delete of one message with a 429 (to prove retry/backoff
//! without failing), marks one message as a system-message 403, and returns 404
//! for an already-gone id. The test then asserts the engine's outcome counts
//! and that the resume callback received every confirmed-gone key.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::delete;
use axum::{Json, Router};
use serde_json::json;

use discord_purge_core::api::DiscordClient;
use discord_purge_core::engine::{ChannelJob, Engine, RunPhase, Stats};

mod support;

#[derive(Default)]
struct MockState {
    deleted: Mutex<Vec<String>>,
    throttled_once: AtomicBool,
}

async fn mock_delete(
    State(st): State<Arc<MockState>>,
    Path((_channel, message)): Path<(String, String)>,
) -> impl IntoResponse {
    // One message is a system message: always 403 with Discord's code 50021.
    if message == "202" {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Cannot execute action on a system message", "code": 50021 })),
        )
            .into_response();
    }
    // One message is already gone.
    if message == "103" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Unknown Message", "code": 10008 })),
        )
            .into_response();
    }
    // The first delete anywhere gets one 429 to exercise the retry path.
    if message == "102" && !st.throttled_once.swap(true, Ordering::SeqCst) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("x-ratelimit-scope", "user")],
            Json(json!({ "message": "rate limited", "retry_after": 0.05, "global": false })),
        )
            .into_response();
    }
    st.deleted.lock().unwrap().push(message);
    StatusCode::NO_CONTENT.into_response()
}

async fn mock_me() -> impl IntoResponse {
    Json(json!({ "id": "acct-e2e", "username": "tester", "discriminator": "0" }))
}

#[tokio::test]
async fn deletes_all_handling_429_404_403() {
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route(
            "/v9/channels/{channel}/messages/{message}",
            delete(mock_delete),
        )
        .route("/v9/channels/{channel}/messages", axum::routing::get(|Path(channel):Path<String>, axum::extract::Query(query):axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            assert_eq!(query["limit"],"1");
            Json(json!([{"id":query["around"],"channel_id":channel,"author":{"id":"acct-e2e"},"pinned":false}]))
        }))
        .route("/v9/users/@me", axum::routing::get(mock_me))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base = format!("http://{addr}");
    let client = DiscordClient::with_base("fake-token", base).unwrap();

    // Verify the client reaches the mock.
    let me = client.current_user().await.unwrap();
    assert_eq!(me.id, "acct-e2e");

    // Build jobs: two channels with normal (101, 201), retry (102), gone (103), and system (202) IDs.
    let jobs = vec![
        ChannelJob {
            channel_id: "c1".into(),
            label: "#one".into(),
            items: vec![101, 102, 103],
            verified: false,
        },
        ChannelJob {
            channel_id: "c2".into(),
            label: "#two".into(),
            items: vec![201, 202],
            verified: false,
        },
    ];
    let total = 5;

    let stats = Stats::new(total, 2);
    let cfg = discord_purge_core::engine::EngineConfig {
        workers: 2,
        delay: Duration::from_millis(1),
        jitter: 0.0,
    };
    let run = support::confirmed(&client, jobs).await;
    let (engine, _controls) = Engine::new(cfg, client, stats.clone());

    let done: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let done2 = done.clone();
    engine
        .run(run, move |k| {
            done2.lock().unwrap().insert(k.to_string());
            Ok(())
        })
        .await;

    let snap = stats.snapshot();
    // 101, 102, 103, 201 count as deleted (103 is 404 = success); 202 is skipped.
    assert_eq!(snap.deleted, 4, "four messages should be confirmed gone");
    assert_eq!(snap.skipped, 1, "the system message should be skipped");
    assert_eq!(snap.failed, 0, "nothing should fail");
    assert_eq!(snap.phase, RunPhase::Completed);
    assert_eq!(
        snap.limit_notice(),
        None,
        "a per-user 429 must not report a reduced limit"
    );
    assert!(
        snap.complete(),
        "a system message needs no retry, so the run is complete"
    );
    assert!(
        snap.status.is_empty(),
        "the 429 wait ended, but the status still says {:?}",
        snap.status
    );

    // Resume keys: deleted + already-gone + system (undeletable forever) = all 5.
    let done = done.lock().unwrap();
    for k in ["101", "102", "103", "201", "202"] {
        assert!(done.contains(k), "resume log should contain {k}");
    }

    // The mock actually received a DELETE for the three truly-deleted ids
    // (102 twice: one 429 then success).
    let deleted = state.deleted.lock().unwrap();
    assert!(deleted.contains(&"101".to_string()));
    assert!(deleted.contains(&"102".to_string()));
    assert!(deleted.contains(&"201".to_string()));
}
