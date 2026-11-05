//! Observable deletion and discovery contracts against a local HTTP server.
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
    filter::{Filter, Kind},
    search::{self, DiscoverConfig},
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Mock {
    mode: &'static str,
    calls: AtomicUsize,
    me_calls: AtomicUsize,
    deleted: Mutex<Vec<String>>,
    searches: Mutex<Vec<HashMap<String, String>>>,
    seen: tokio::sync::Notify,
}

async fn inspect(
    State(st): State<Arc<Mock>>,
    Path(channel): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(query.get("limit").map(String::as_str), Some("1"));
    let message = query.get("around").unwrap();
    Json(
        json!([{"id":message,"channel_id":channel,"author":{"id": if st.mode == "protected" && message == "201" {"8"} else {"7"}},
        "pinned": st.mode == "protected" && message == "202"}]),
    )
    .into_response()
}
async fn remove(
    State(st): State<Arc<Mock>>,
    Path((_, message)): Path<(String, String)>,
) -> Response {
    let n = st.calls.fetch_add(1, Ordering::SeqCst);
    st.seen.notify_one();
    if st.mode == "unauthorized" {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if st.mode == "stop" || (st.mode == "collapse" && n < 2) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("x-ratelimit-scope", "global"),
                (
                    "retry-after",
                    if st.mode == "stop" { "86400" } else { "0.001" },
                ),
            ],
            Json(json!({"retry_after":if st.mode == "stop" {86400.0} else {0.001},"global":true})),
        )
            .into_response();
    }
    st.deleted.lock().unwrap().push(message);
    StatusCode::NO_CONTENT.into_response()
}
async fn search_page(
    State(st): State<Arc<Mock>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut requests = st.searches.lock().unwrap();
    requests.push(query.clone());
    if st.mode == "deep" {
        // 10,050 results: the first 400 pages fill the offset window, then
        // one more result remains below the oldest ID.
        let offset: u64 = query["offset"].parse().unwrap();
        let (id, total) = match query.get("max_id") {
            None => (100_000 - offset, 10_050),
            Some(max) => (max.parse::<u64>().unwrap() - 1, 1),
        };
        return Json(json!({"total_results":total,"messages":[
            [{"id":id.to_string(),"channel_id":"10","type":0,"content":"old","hit":true,"author":{"id":"7"}}]
        ]}))
        .into_response();
    }
    let page = if requests.len() == 1 || st.mode == "repeat" {
        // A short page: Discord can return fewer than 25 results mid-range.
        json!({"total_results":26,"messages":[
            [{"id":"104","channel_id":"10","type":0,"content":"context","hit":false,"author":{"id":"7"}}],
            [{"id":"103","channel_id":"10","type":0,"content":"foreign","hit":true,"author":{"id":"8"}}],
            [{"id":"102","channel_id":"10","type":0,"content":"keep image","hit":true,"author":{"id":"7"},"attachments":[{"filename":"photo.png"}]}],
            [{"id":"101","channel_id":"10","type":0,"content":"keep pin","hit":true,"pinned":true,"author":{"id":"7"}}]
        ]})
    } else if st.mode == "search" && requests.len() == 2 && requests[1]["offset"] == "25" {
        json!({"total_results":26,"messages":[[{"id":"100","channel_id":"10","type":0,"content":"keep adjacent","hit":true,"author":{"id":"7"},"attachments":[{"filename":"adjacent.png"}]}]]})
    } else {
        json!({"total_results":0,"messages":[]})
    };
    Json(page).into_response()
}
async fn server(mode: &'static str) -> (DiscordClient, Arc<Mock>, tokio::task::JoinHandle<()>) {
    let st = Arc::new(Mock {
        mode,
        calls: AtomicUsize::new(0),
        me_calls: AtomicUsize::new(0),
        deleted: Mutex::new(Vec::new()),
        searches: Mutex::new(Vec::new()),
        seen: tokio::sync::Notify::new(),
    });
    let app = Router::new()
        .route(
            "/v9/users/@me",
            get(|State(st): State<Arc<Mock>>| async move {
                st.me_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({"id":"7","username":"test"}))
            }),
        )
        .route(
            "/v9/channels/{channel}/messages/{message}",
            axum::routing::delete(remove),
        )
        .route("/v9/channels/{channel}/messages", get(inspect))
        .route("/v9/channels/{channel}/messages/search", get(search_page))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = DiscordClient::with_base("test-token", base).unwrap();
    client.current_user().await.unwrap();
    (client, st, task)
}
fn job(channel: usize, messages: &[u64]) -> ChannelJob {
    ChannelJob {
        channel_id: channel.to_string(),
        label: channel.to_string(),
        items: messages.to_vec(),
        verified: false,
    }
}
fn config(workers: usize) -> EngineConfig {
    EngineConfig {
        workers,
        delay: Duration::ZERO,
        jitter: 0.0,
    }
}

#[tokio::test]
async fn all_channels_finish_after_concurrency_drops() {
    let (client, st, server) = server("collapse").await;
    let jobs = (1..=6).map(|n| job(n, &[1, 2])).collect();
    let stats = Stats::new(12, 3);
    let (engine, _) = Engine::new(config(3), client, stats.clone());
    tokio::time::timeout(Duration::from_secs(6), engine.run(jobs, |_| Ok(())))
        .await
        .expect("all queued channels must finish after collapse");
    assert_eq!(stats.snapshot().active_limit, 1);
    assert_eq!(stats.snapshot().deleted, 12);
    assert_eq!(st.deleted.lock().unwrap().len(), 12);
    server.abort();
}

#[tokio::test]
async fn invalid_token_stops_further_deletes() {
    let (client, st, server) = server("unauthorized").await;
    let stats = Stats::new(3, 1);
    let (engine, _) = Engine::new(config(1), client, stats.clone());
    engine.run(vec![job(1, &[1, 2, 3])], |_| Ok(())).await;
    assert_eq!(st.calls.load(Ordering::SeqCst), 1);
    assert!(stats.snapshot().aborted);
    assert_eq!(stats.snapshot().failed, 1);
    server.abort();
}

#[tokio::test]
async fn author_and_pin_checks_block_unreviewed_deletion() {
    let (client, st, server) = server("protected").await;
    let stats = Stats::new(3, 1);
    let (engine, _) = Engine::new(config(1), client.clone(), stats.clone());
    let completed = Arc::new(Mutex::new(Vec::new()));
    let recorded = completed.clone();
    engine
        .run(vec![job(1, &[201, 202, 203])], move |key| {
            recorded.lock().unwrap().push(key.to_string());
            Ok(())
        })
        .await;
    assert_eq!(*st.deleted.lock().unwrap(), vec!["203"]);
    assert_eq!(*completed.lock().unwrap(), vec!["203"]);
    assert_eq!(stats.snapshot().skipped, 2);
    let outcome = client
        .include_pinned(true)
        .delete_message("1", "202", false)
        .await;
    assert!(matches!(
        outcome,
        discord_purge_core::api::DeleteOutcome::Deleted
    ));
    assert_eq!(*st.deleted.lock().unwrap(), vec!["203", "202"]);
    server.abort();
}

#[tokio::test]
async fn stop_interrupts_a_long_rate_limit_wait() {
    let (client, st, server) = server("stop").await;
    let stats = Stats::new(2, 1);
    let (engine, controls) = Engine::new(config(1), client, stats.clone());
    let task = tokio::spawn(async move {
        engine.run(vec![job(1, &[1, 2])], |_| Ok(())).await;
    });
    st.seen.notified().await;
    controls.stop();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(st.calls.load(Ordering::SeqCst), 1);
    assert!(stats.snapshot().aborted);
    assert_eq!(stats.snapshot().processed, 0);
    server.abort();
}

#[tokio::test]
async fn resume_write_error_stops_new_deletes() {
    let (client, st, server) = server("normal").await;
    let stats = Stats::new(2, 1);
    let (engine, _) = Engine::new(config(1), client, stats.clone());
    engine
        .run(vec![job(1, &[1, 2])], |_| anyhow::bail!("disk full"))
        .await;
    assert_eq!(st.calls.load(Ordering::SeqCst), 1);
    assert!(stats.snapshot().aborted);
    assert!(
        stats
            .snapshot()
            .errors
            .iter()
            .any(|e| e.contains("disk full"))
    );
    server.abort();
}

#[tokio::test]
async fn search_applies_author_hit_attachment_and_local_regex_filters() {
    let (client, st, server) = server("search").await;
    let cfg = DiscoverConfig {
        author_id: "7".into(),
        channel_ids: vec!["10".into()],
        search_delay: Duration::ZERO,
        ..Default::default()
    };
    let filter = Filter {
        content: Filter::parse_content("/^keep/").unwrap(),
        types: Kind::IMAGE,
        ..Default::default()
    };
    let jobs = search::discover(&client, &cfg, &filter).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].items, vec![102, 100]);
    let requests = st.searches.lock().unwrap();
    // A short first page does not end the walk; total_results does.
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["offset"], "0");
    assert_eq!(requests[1]["offset"], "25");
    assert!(requests.iter().all(|r| !r.contains_key("max_id")));
    assert!(!requests[0].contains_key("content"));
    assert_eq!(requests[0]["author_id"], "7");
    drop(requests);
    server.abort();
}

#[tokio::test]
async fn search_continues_below_the_oldest_id_past_the_offset_limit() {
    let (client, st, server) = server("deep").await;
    let cfg = DiscoverConfig {
        author_id: "7".into(),
        channel_ids: vec!["10".into()],
        search_delay: Duration::ZERO,
        ..Default::default()
    };
    let jobs = search::discover(&client, &cfg, &Filter::default())
        .await
        .unwrap();
    assert_eq!(jobs[0].items.len(), 401);
    let requests = st.searches.lock().unwrap();
    assert_eq!(requests[399]["offset"], "9975");
    let last = requests.last().unwrap();
    assert_eq!(last["offset"], "0");
    assert_eq!(last["max_id"], "90025");
    drop(requests);
    server.abort();
}

#[tokio::test]
async fn repeated_search_page_returns_an_error() {
    let (client, _, server) = server("repeat").await;
    let cfg = DiscoverConfig {
        author_id: "7".into(),
        channel_ids: vec!["10".into()],
        search_delay: Duration::ZERO,
        ..Default::default()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        search::discover(&client, &cfg, &Filter::default()),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("repeated a page"));
    server.abort();
}

#[tokio::test]
async fn package_preview_is_offline_even_with_a_token() {
    let (client, st, server) = server("normal").await;
    let before = st.me_calls.load(Ordering::SeqCst);
    let source = discord_purge_core::Source::Package(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/package.zip"),
    );
    let preview = discord_purge_core::prepare(&client, &source, &Filter::default())
        .await
        .unwrap();
    assert_eq!(preview.total, 50);
    assert_eq!(preview.account_id, "unknown");
    assert_eq!(
        st.me_calls.load(Ordering::SeqCst),
        before,
        "package preview must not contact Discord"
    );
    server.abort();
}

#[test]
fn cli_rejects_invalid_limits_and_keeps_package_preview_offline() {
    let state = tempfile::tempdir().unwrap();
    for option in [
        "--workers=0",
        "--workers=17",
        "--delay=NaN",
        "--delay=inf",
        "--jitter=NaN",
        "--jitter=1.1",
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_discord-purge"))
            .args([
                "--no-tui",
                "--package",
                "../../tests/fixtures/package.zip",
                option,
            ])
            .env_remove("DISCORD_TOKEN")
            .env("DISCORD_PURGE_STATE_DIR", state.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{option} must be rejected");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Use"),
            "the error must state the accepted input"
        );
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_discord-purge"))
        .args(["--no-tui", "--package"])
        .arg(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/package.zip"),
        )
        .arg("--type=image")
        .current_dir(state.path())
        .env_remove("DISCORD_PURGE_CLIENT_HAR")
        .env_remove("DISCORD_TOKEN")
        .env("DISCORD_PURGE_STATE_DIR", state.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing was deleted"));
    assert_eq!(
        std::fs::read_dir(state.path()).unwrap().count(),
        0,
        "preview must not write state"
    );
}
