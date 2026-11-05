//! User-account verification must use history-around, never the bot-only GET.
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use discord_purge_core::{
    api::DiscordClient,
    engine::{ChannelJob, Engine, EngineConfig, Stats},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Fixture {
    mode: &'static str,
    calls: Mutex<Vec<String>>,
    deleted: Mutex<Vec<String>>,
}
async fn bot_only(State(st): State<Arc<Fixture>>) -> Response {
    st.calls.lock().unwrap().push("bot-only GET".into());
    (
        StatusCode::FORBIDDEN,
        Json(json!({"code":20002,"message":"Only bots can use this endpoint"})),
    )
        .into_response()
}
async fn history(
    State(st): State<Arc<Fixture>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    assert_eq!(q.get("limit").map(String::as_str), Some("1"));
    let target = q
        .get("around")
        .expect("verification must scope history to the target message");
    st.calls.lock().unwrap().push(format!("history {target}"));
    if st.mode == "endpoint" {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"code":20002,"message":"Only bots can use this endpoint"})),
        )
            .into_response();
    }
    if st.mode == "access" {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"code":50001,"message":"Missing Access"})),
        )
            .into_response();
    }
    if st.mode == "empty" {
        return Json(json!([])).into_response();
    }
    let mut message = json!({"id":if st.mode=="neighbor" {"999"}else{target},"channel_id":"10","author":{"id":if st.mode=="foreign"{"8"}else{"7"}},"pinned":st.mode=="pinned"});
    if st.mode == "missing-pin" {
        message.as_object_mut().unwrap().remove("pinned");
    }
    Json(json!([message])).into_response()
}
async fn remove(
    State(st): State<Arc<Fixture>>,
    Path((_, message)): Path<(String, String)>,
) -> Response {
    st.deleted.lock().unwrap().push(message);
    StatusCode::NO_CONTENT.into_response()
}
async fn server(mode: &'static str) -> (DiscordClient, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let st = Arc::new(Fixture {
        mode,
        calls: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route(
            "/v9/users/@me",
            get(|| async { Json(json!({"id":"7","username":"owner"})) }),
        )
        .route("/v9/channels/{channel}/messages", get(history))
        .route(
            "/v9/channels/{channel}/messages/{message}",
            get(bot_only).delete(remove),
        )
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        DiscordClient::with_base("test-token", base).unwrap(),
        st,
        task,
    )
}
fn job(count: usize) -> ChannelJob {
    ChannelJob {
        channel_id: "10".into(),
        label: "test channel".into(),
        items: (20..20 + count as u64).collect(),
        verified: false,
    }
}
fn config() -> EngineConfig {
    EngineConfig {
        workers: 1,
        delay: Duration::ZERO,
        jitter: 0.0,
    }
}

#[tokio::test]
async fn user_account_deletes_after_exact_history_verification() {
    let (client, st, server) = server("normal").await;
    let stats = Stats::new(2, 1);
    let (engine, _) = Engine::new(config(), client.clone(), stats.clone());
    let done = Arc::new(Mutex::new(Vec::new()));
    let completed = done.clone();
    engine
        .run(vec![job(2)], move |key| {
            completed.lock().unwrap().push(key.to_string());
            Ok(())
        })
        .await;
    assert_eq!(
        stats.snapshot().deleted,
        2,
        "the user-account check must not block the entire channel"
    );
    assert_eq!(stats.snapshot().skipped, 0);
    assert_eq!(*st.deleted.lock().unwrap(), vec!["20", "21"]);
    assert_eq!(*done.lock().unwrap(), vec!["20", "21"]);
    let selected = client.message("10", "20").await.unwrap();
    assert_eq!(selected.id, "20");
    assert!(!st.calls.lock().unwrap().iter().any(|s| s == "bot-only GET"));
    server.abort();
}

#[tokio::test]
async fn unverified_foreign_and_pinned_history_never_authorizes_delete() {
    for mode in ["neighbor", "empty", "foreign", "pinned", "missing-pin"] {
        let (client, st, server) = server(mode).await;
        let stats = Stats::new(1, 1);
        let (engine, _) = Engine::new(config(), client, stats.clone());
        let done = Arc::new(Mutex::new(Vec::new()));
        let completed = done.clone();
        engine
            .run(vec![job(1)], move |key| {
                completed.lock().unwrap().push(key.to_string());
                Ok(())
            })
            .await;
        assert!(
            st.deleted.lock().unwrap().is_empty(),
            "{mode} must never authorize deletion"
        );
        // History centred on the ID without it means the message is gone.
        let gone = matches!(mode, "neighbor" | "empty");
        assert_eq!(
            *done.lock().unwrap(),
            if gone { vec!["20".to_string()] } else { vec![] },
            "{mode}: only a missing message is recorded as finished"
        );
        assert_eq!(stats.snapshot().deleted, gone as i64);
        server.abort();
    }
}

#[tokio::test]
async fn verified_jobs_delete_without_a_history_check() {
    let (client, st, server) = server("foreign").await;
    let stats = Stats::new(2, 1);
    let (engine, _) = Engine::new(config(), client, stats.clone());
    let mut verified = job(2);
    verified.verified = true;
    engine.run(vec![verified], |_| Ok(())).await;
    assert_eq!(*st.deleted.lock().unwrap(), vec!["20", "21"]);
    assert!(st.calls.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn endpoint_error_aborts_and_access_error_defers_without_per_item_waits() {
    for mode in ["endpoint", "access"] {
        let (client, st, server) = server(mode).await;
        let stats = Stats::new(31, 1);
        let mut cfg = config();
        cfg.delay = Duration::from_secs(120);
        let (engine, _) = Engine::new(cfg, client, stats.clone());
        let done = Arc::new(Mutex::new(Vec::new()));
        let completed = done.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            engine.run(vec![job(31)], move |key| {
                completed.lock().unwrap().push(key.to_string());
                Ok(())
            }),
        )
        .await
        .expect("a blocked/fatal endpoint must not spend two minutes per skipped item");
        assert!(st.deleted.lock().unwrap().is_empty());
        assert!(done.lock().unwrap().is_empty());
        assert_eq!(
            st.calls.lock().unwrap().len(),
            1,
            "stop repeated requests to a blocked/unsupported endpoint"
        );
        let snapshot = stats.snapshot();
        if mode == "endpoint" {
            assert!(snapshot.aborted);
            assert_eq!(snapshot.failed, 1);
            assert_eq!(snapshot.skipped, 0);
            assert!(snapshot.errors.iter().any(|e| e.contains("Only bots")));
        } else {
            assert!(!snapshot.aborted);
            assert_eq!(snapshot.skipped, 31);
            assert_eq!(snapshot.failed, 0);
            assert!(snapshot.status.contains("Missing Access"));
            assert!(snapshot.errors.iter().any(|e| e.contains("Missing Access")));
            assert_eq!(snapshot.forbidden[0].reason, "Missing Access");
        }
        server.abort();
    }
}
