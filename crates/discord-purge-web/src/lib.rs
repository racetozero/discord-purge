//! Local web panel (axum + Server-Sent Events).
//!
//! `--web` starts a server on 127.0.0.1 and opens the browser. The page offers
//! the same flow as the TUI — enter a token, preview, confirm, run — against
//! the same engine. Progress streams to the page over SSE. The server binds to
//! loopback only, so nothing is exposed off the machine. Unless the panel starts
//! with `--no-save-token`, a token that Discord accepts is saved for all front ends.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use discord_purge_core::api::{DEFAULT_BASE, DiscordClient, TokenRejected};
use discord_purge_core::engine::{Controls, Engine, EngineConfig, Stats};
use discord_purge_core::search::INCOMPLETE_NOTICE;
use discord_purge_core::selection::{self, Selection};
use discord_purge_core::{Options, PreparedRun, Source, prepare, resume};

struct Live {
    /// Names this run in `/api/run` and in each event, so the page can tell its
    /// own run from an earlier one.
    id: String,
    stats: Arc<Stats>,
    controls: Controls,
    account: String,
}

struct AppState {
    defaults: Options,
    live: Mutex<Option<Live>>,
    preview: Mutex<Option<StoredPreview>>,
    run_lock: Mutex<()>,
    session: String,
    host: String,
    api_base: String,
}

struct StoredPreview {
    id: String,
    run: PreparedRun,
    include_pinned: bool,
}

impl AppState {
    async fn run_active(&self) -> bool {
        self.live
            .lock()
            .await
            .as_ref()
            .is_some_and(|run| !run.stats.is_finished())
    }

    /// Allow one preview or run preparation at a time, and none during a run.
    async fn admit(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, ApiError> {
        let Ok(guard) = self.run_lock.try_lock() else {
            return Err(
                "A preview or run is being prepared. Wait for it to finish and retry.".into(),
            );
        };
        if self.run_active().await {
            return Err("A run is active. Stop it or wait for completion before you retry.".into());
        }
        Ok(guard)
    }

    /// The request's token, then `--token`, then the saved token. The saved
    /// token is read on each request, so a token that the CLI or TUI saves
    /// while the panel runs is used too.
    fn token(&self, token: &Option<String>) -> Option<String> {
        pick(token, &self.defaults.token).or_else(|| self.defaults.token_store.load())
    }

    /// Save a token that Discord accepted. Returns whether it is now saved; a
    /// failed save does not stop the request, so it only goes to the console.
    fn remember(&self, token: &str) -> bool {
        match self.defaults.token_store.remember(token) {
            Ok(saved) => saved,
            Err(error) => {
                eprintln!("warning: {error}");
                false
            }
        }
    }

    fn package(&self, package: &Option<String>) -> Option<PathBuf> {
        let default = self
            .defaults
            .package
            .as_ref()
            .map(|p| p.display().to_string());
        pick(package, &default).map(PathBuf::from)
    }
}

pub async fn serve(config: Options, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let port = listener.local_addr()?.port();
    let session = format!("{:032x}", rand::thread_rng().r#gen::<u128>());
    let cfg_store = config.token_store;
    let state = Arc::new(AppState {
        defaults: config,
        live: Mutex::new(None),
        preview: Mutex::new(None),
        run_lock: Mutex::new(()),
        session: session.clone(),
        host: format!("127.0.0.1:{port}"),
        api_base: DEFAULT_BASE.into(),
    });

    let app = router(state);

    let url = format!("http://127.0.0.1:{port}/#session={session}");
    eprintln!("discord-purge web panel at {url}");
    eprintln!(
        "It deletes only your own messages. {}",
        match cfg_store {
            resume::TokenStore::ReadWrite =>
                "A token that Discord accepts is saved for the CLI and TUI too.",
            _ => "It keeps a new token in memory only.",
        }
    );
    // Auto-open the browser unless asked not to (handy for headless/test use).
    if std::env::var_os("DISCORD_PURGE_NO_OPEN").is_none()
        && let Err(error) = open::that(&url)
    {
        eprintln!("Cannot open a browser: {error}. Open the URL above yourself.");
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/defaults", get(defaults))
        .route("/api/catalog", post(browse_catalog))
        .route("/api/channels", post(browse_channels))
        .route("/api/messages", post(browse_messages))
        .route("/api/preview", post(preview))
        .route("/api/run", post(run_handler))
        .route("/api/control", post(control))
        .route("/api/events", get(events))
        .layer(middleware::from_fn_with_state(state.clone(), protect))
        .with_state(state)
}

async fn protect(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> axum::response::Response {
    let host = request.headers().get("host").and_then(|v| v.to_str().ok());
    let origin = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok());
    let expected_origin = format!("http://{}", state.host);
    if host != Some(state.host.as_str())
        || origin.is_some_and(|other_origin| other_origin != expected_origin)
    {
        return (
            StatusCode::FORBIDDEN,
            "Use the local panel URL printed by this app.",
        )
            .into_response();
    }
    if request.uri().path().starts_with("/api/") {
        let key = request
            .headers()
            .get("x-discord-purge-session")
            .and_then(|v| v.to_str().ok());
        let event_key = request
            .uri()
            .query()
            .and_then(|query| query.strip_prefix("session="));
        let valid = key == Some(state.session.as_str())
            || (request.uri().path() == "/api/events" && event_key == Some(state.session.as_str()));
        if !valid {
            return (
                StatusCode::FORBIDDEN,
                "The panel session is missing or invalid. Open the full URL printed by this app.",
            )
                .into_response();
        }
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn defaults(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let inputs = state.defaults.filter.inputs();
    Json(json!({
        "package": state.defaults.package.as_ref().map(|p| p.display().to_string()),
        "guild": state.defaults.guild, "channel": state.defaults.channel,
        "content": inputs.content, "types": inputs.types,
        "after_date": inputs.after_date, "before_date": inputs.before_date,
        "include_pinned": state.defaults.include_pinned,
        "workers": state.defaults.settings.workers, "delay": state.defaults.settings.delay.as_secs_f64(),
        "jitter": state.defaults.settings.jitter, "has_token": state.token(&None).is_some(),
        "saves_token": state.defaults.token_store == resume::TokenStore::ReadWrite,
        "token_help": discord_purge_core::TOKEN_HELP,
    }))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

#[derive(Deserialize)]
struct PreviewReq {
    token: Option<String>,
    selection: Option<Selection>,
    package: Option<String>,
    guild: Option<String>,
    channel: Option<String>,
    content: Option<String>,
    types: Option<String>,
    after_date: Option<String>,
    before_date: Option<String>,
    include_pinned: Option<bool>,
}

/// The first non-empty value: an empty field in a request falls back to the
/// default.
fn pick(request: &Option<String>, default_value: &Option<String>) -> Option<String> {
    [request, default_value]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

#[derive(Deserialize)]
struct BrowseReq {
    token: Option<String>,
    package: Option<String>,
    #[serde(default)]
    guild_ids: Vec<String>,
    channel_id: Option<String>,
    before: Option<String>,
}

async fn browse_source(
    state: &AppState,
    request: &BrowseReq,
) -> anyhow::Result<(DiscordClient, Option<std::path::PathBuf>, String)> {
    anyhow::ensure!(
        !state.run_active().await,
        "A run is active. Stop it or wait for completion before browsing another selection."
    );
    let token = state.token(&request.token).unwrap_or_default();
    Ok((
        DiscordClient::with_base(token.clone(), state.api_base.clone())?,
        state.package(&request.package),
        token,
    ))
}

struct ApiError {
    message: String,
    token_rejected: bool,
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self {
            message: error.to_string(),
            token_rejected: false,
        }
    }
}

impl ApiError {
    /// An error from a request that used the token. The page must know about a
    /// rejection, so it stops offering deletion with that token.
    fn from_account(error: anyhow::Error) -> Self {
        Self {
            token_rejected: error.is::<TokenRejected>(),
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false, "error": self.message, "token_rejected": self.token_rejected,
            })),
        )
            .into_response()
    }
}

type ApiResult = Result<axum::response::Response, ApiError>;

async fn browse_catalog(
    State(state): State<Arc<AppState>>,
    Json(request): Json<BrowseReq>,
) -> ApiResult {
    let (client, package, token) = browse_source(&state, &request).await?;
    let catalog = selection::catalog(&client, package.as_deref())
        .await
        .map_err(ApiError::from_account)?;
    // Without a package, the catalog came from Discord, so it accepted the token.
    let saved = package.is_none() && state.remember(&token);
    Ok(Json(json!({
        "ok": true, "account": catalog.account, "guilds": catalog.guilds,
        "dms": catalog.dms, "token_saved": saved,
    }))
    .into_response())
}

async fn browse_channels(
    State(state): State<Arc<AppState>>,
    Json(request): Json<BrowseReq>,
) -> ApiResult {
    let (client, package, _) = browse_source(&state, &request).await?;
    let channels = selection::channels(&client, package.as_deref(), &request.guild_ids).await?;
    Ok(Json(json!({"ok":true,"channels":channels})).into_response())
}

async fn browse_messages(
    State(state): State<Arc<AppState>>,
    Json(request): Json<BrowseReq>,
) -> ApiResult {
    let (client, package, _) = browse_source(&state, &request).await?;
    let channel = request
        .channel_id
        .as_deref()
        .ok_or("You selected no history channel. Select one channel or DM and retry.")?;
    let messages = selection::messages(
        &client,
        package.as_deref(),
        channel,
        request.before.as_deref(),
    )
    .await?;
    let more = messages.len() == discord_purge_core::api::HISTORY_PAGE;
    Ok(Json(json!({"ok":true,"messages":messages,"more":more})).into_response())
}

async fn preview(State(state): State<Arc<AppState>>, Json(request): Json<PreviewReq>) -> ApiResult {
    // A dry-run preview needs no token for a package (nothing is deleted and no
    // account is required), matching the headless offline preview. The run
    // endpoint still requires a token for a real deletion.
    let client = DiscordClient::with_base(
        state.token(&request.token).unwrap_or_default(),
        state.api_base.clone(),
    )?;
    let include_pinned = request
        .include_pinned
        .unwrap_or(state.defaults.include_pinned);
    let package = state.package(&request.package);
    let source = if let Some(mut selection) = request.selection.clone() {
        selection.include_pinned = include_pinned;
        selection.validate()?;
        Source::Selected { selection, package }
    } else {
        let guild = pick(&request.guild, &state.defaults.guild);
        let channel = pick(&request.channel, &state.defaults.channel);
        Source::from_inputs(package, guild, channel, include_pinned)?
    };
    let filter = state.defaults.filter.clone().with_inputs(
        request.content.as_deref(),
        request.types.as_deref(),
        request.after_date.as_deref(),
        request.before_date.as_deref(),
    )?;
    let _guard = state.admit().await?;
    let scope = match &source {
        Source::Selected { selection, .. } => selection.description(),
        _ => "All matching messages in the specified source".into(),
    };
    let run = prepare(&client, &source, &filter)
        .await
        .map_err(ApiError::from_account)?;
    let preview_id = format!("{:032x}", rand::thread_rng().r#gen::<u128>());
    let channels: Vec<_> = run
        .jobs
        .iter()
        .take(500)
        .map(|job| json!({ "label": job.label, "channel_id": job.channel_id, "count": job.items.len() }))
        .collect();
    let response = Json(json!({
        "ok": true,
        "account": run.account_label,
        "total": run.total,
        "channels": channels,
        "preview_id": preview_id,
        "scope": scope,
        "incomplete": (!run.complete).then_some(INCOMPLETE_NOTICE),
    }))
    .into_response();
    *state.preview.lock().await = Some(StoredPreview {
        id: preview_id,
        run,
        include_pinned,
    });
    Ok(response)
}

#[derive(Deserialize)]
struct RunReq {
    token: Option<String>,
    preview_id: String,
    #[serde(default)]
    execute: bool,
    workers: Option<usize>,
    delay: Option<f64>,
    jitter: Option<f64>,
}

async fn run_handler(State(state): State<Arc<AppState>>, Json(request): Json<RunReq>) -> ApiResult {
    let _guard = state.admit().await?;
    if !request.execute {
        return Err(
            "You did not confirm deletion. Complete a preview and set execute to true.".into(),
        );
    }
    let defaults = &state.defaults.settings;
    let settings = EngineConfig::new(
        request.workers.unwrap_or(defaults.workers),
        request.delay.unwrap_or(defaults.delay.as_secs_f64()),
        request.jitter.unwrap_or(defaults.jitter),
    )?;
    let token = state
        .token(&request.token)
        .ok_or("You did not supply a token. Enter your account token before you start deletion.")?;
    let client = DiscordClient::with_base(token.clone(), state.api_base.clone())?;
    // Hold the preview until admission completes. A failed check does not consume it.
    let mut preview = state.preview.lock().await;
    let stored = preview
        .as_ref()
        .filter(|stored| stored.id == request.preview_id)
        .ok_or("The preview is missing or has changed. Complete a new preview before deletion.")?;
    let client = client.include_pinned(stored.include_pinned);
    let run = stored
        .run
        .confirm(&client)
        .await
        .map_err(ApiError::from_account)?;
    state.remember(&token);
    let log = resume::ResumeLog::open(&run.account_id).map_err(|error| {
        format!("Cannot open the resume log: {error}. Check state directory permissions and retry.")
    })?;
    preview.take();
    let (total, already_done) = (run.total, run.already_done);
    let account = run.account_label.clone();
    let run_id = format!("{:032x}", rand::thread_rng().r#gen::<u128>());
    let stats = Stats::new(total, settings.workers);
    let (engine, controls) = Engine::new(settings, client, stats.clone());
    *state.live.lock().await = Some(Live {
        id: run_id.clone(),
        stats: stats.clone(),
        controls,
        account: account.clone(),
    });
    tokio::spawn(async move {
        engine.run(run, move |key| log.record(key)).await;
        if let Err(error) = resume::write_report(&account, &stats.snapshot(), None) {
            eprintln!("Cannot write the report: {error}. Check state directory permissions.");
        }
    });
    Ok(Json(json!({
        "ok": true, "total": total, "already_done": already_done, "run_id": run_id,
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct ControlReq {
    action: String,
}

async fn control(State(state): State<Arc<AppState>>, Json(request): Json<ControlReq>) -> ApiResult {
    let live = state.live.lock().await;
    let run = live
        .as_ref()
        .ok_or("You sent a run action, but no run exists. Start a run first.")?;
    if run.stats.is_finished() {
        return Err(
            "You sent a run action, but this run has ended. Select New selection to start another run."
                .into(),
        );
    }
    match request.action.as_str() {
        "pause" => run.controls.pause(),
        "resume" => run.controls.resume(),
        "stop" => run.controls.stop(),
        other => {
            return Err(format!("unknown action {other:?}. Use pause, resume, or stop.").into());
        }
    }
    Ok(Json(json!({ "ok": true })).into_response())
}

fn snapshot_json(run: &Live) -> serde_json::Value {
    let snapshot = run.stats.snapshot();
    json!({
        "run_id": run.id,
        "account": run.account,
        "phase": snapshot.phase,
        "label": snapshot.phase.label(),
        "total": snapshot.total,
        "deleted": snapshot.deleted,
        "skipped": snapshot.skipped,
        "deferred": snapshot.deferred,
        "failed": snapshot.failed,
        "processed": snapshot.processed,
        "rate": snapshot.rate,
        "elapsed_secs": snapshot.elapsed.as_secs(),
        "eta_secs": snapshot.eta.map(|d| d.as_secs()),
        "status": snapshot.status,
        "limit_notice": snapshot.limit_notice(),
        "workers": snapshot.workers.iter().filter(|worker| worker.active).map(|worker| json!({
            "channel": worker.channel, "done": worker.done, "total": worker.total
        })).collect::<Vec<_>>(),
        "errors": snapshot.errors.iter().rev().take(8).cloned().collect::<Vec<_>>(),
        "forbidden": snapshot.forbidden.iter().map(|f| json!({
            "label": f.label, "count": f.count, "reason": f.reason
        })).collect::<Vec<_>>(),
    })
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let stream = stream::unfold(state, |state| async move {
        let payload = state.live.lock().await.as_ref().map(snapshot_json);
        let event = match payload {
            Some(p) => SseEvent::default().data(p.to_string()),
            None => SseEvent::default().event("idle").data("{}"),
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        Some((Ok(event), state))
    });
    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use discord_purge_core::filter::Filter;

    #[tokio::test]
    async fn local_panel_requires_session_and_previews_exact_jobs() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let state = Arc::new(AppState {
            defaults: Options {
                token: None,
                package: Some(
                    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests/fixtures/package.zip"),
                ),
                guild: None,
                channel: None,
                filter: Filter::default(),
                settings: Default::default(),
                include_pinned: false,
                token_store: discord_purge_core::resume::TokenStore::Off,
            },
            live: Mutex::new(None),
            preview: Mutex::new(None),
            run_lock: Mutex::new(()),
            session: "test-session".into(),
            host: host.clone(),
            api_base: DEFAULT_BASE.into(),
        });
        let app = router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let url = format!("http://{host}");
        let index = client.get(&url).send().await.unwrap();
        assert_eq!(index.status(), 200);
        assert_eq!(index.headers()["x-frame-options"], "DENY");
        let page = index.text().await.unwrap();
        for landmark in [r#"id="where""#, r#"id="messages""#, r#"id="plan""#] {
            assert!(
                page.contains(landmark),
                "the page must show the {landmark} pane"
            );
        }
        // Password managers offer to save any password field, so the token
        // field must be a masked text field with each manager's opt-out.
        assert!(
            !page.contains(r#"type="password""#),
            "the page must not have a password field"
        );
        let token_field = page
            .lines()
            .find(|line| line.contains(r#"<input id="token""#))
            .unwrap();
        for opt_out in [
            r#"type="text""#,
            r#"autocomplete="off""#,
            "data-1p-ignore",
            r#"data-lpignore="true""#,
            "data-bwignore",
            "data-protonpass-ignore",
            r#"data-form-type="other""#,
            "keeper-ignore",
        ] {
            assert!(
                token_field.contains(opt_out),
                "the token field must carry {opt_out}"
            );
        }
        assert!(!token_field.to_lowercase().contains("password"));
        let defaults: serde_json::Value = client
            .get(format!("{url}/api/defaults"))
            .header("x-discord-purge-session", "test-session")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            defaults["token_help"]
                .as_str()
                .unwrap()
                .contains("authorization")
        );
        assert_eq!(defaults["content"], "");
        assert_eq!(defaults["types"], "");
        assert_eq!(
            client
                .post(format!("{url}/api/preview"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .post(format!("{url}/api/preview"))
                .header("x-discord-purge-session", "test-session")
                .header("origin", "https://example.com")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .get(&url)
                .header("host", "attacker.example")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let invalid = client
            .post(format!("{url}/api/preview"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"content":"/[/"}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), 400);
        let result: serde_json::Value = client
            .post(format!("{url}/api/preview"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["total"], 50);
        assert_eq!(result["channels"].as_array().unwrap().len(), 2);
        let stored = state.preview.lock().await;
        assert_eq!(
            stored.as_ref().unwrap().id,
            result["preview_id"].as_str().unwrap()
        );
        assert_eq!(
            stored
                .as_ref()
                .unwrap()
                .run
                .jobs
                .iter()
                .map(|j| j.items.len())
                .sum::<usize>(),
            50
        );
        drop(stored);
        let result = client
            .post(format!("{url}/api/run"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"token":"fake", "execute":true,"preview_id":"stale"}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 400);
        assert!(
            state.preview.lock().await.is_some(),
            "failed admission must preserve preview"
        );
        for endpoint in ["catalog", "channels", "messages"] {
            let response = client
                .post(format!("{url}/api/{endpoint}"))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                403,
                "new browsing endpoints require the panel session"
            );
        }
        let catalog: serde_json::Value = client
            .post(format!("{url}/api/catalog"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog["guilds"][0]["id"], "900");
        assert_eq!(catalog["dms"][0]["id"], "222");
        let channels: serde_json::Value = client
            .post(format!("{url}/api/channels"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"guild_ids":["900"]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(channels["channels"][0]["id"], "111");
        let history: serde_json::Value = client
            .post(format!("{url}/api/messages"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"channel_id":"111"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let chosen = history["messages"][0]["id"].as_str().unwrap();
        let refs = vec![json!({"channel_id":"111","message_id":chosen})];
        let response:serde_json::Value=client.post(format!("{url}/api/preview")).header("x-discord-purge-session","test-session").json(&json!({"selection":{"targets":[{"kind":"guild","guild_id":"900","channel":null},{"kind":"dm","channel_id":"222"}],"range":{"kind":"only","messages":refs}}})).send().await.unwrap().json().await.unwrap();
        assert_eq!(response["total"], 1);
        assert!(
            response["scope"]
                .as_str()
                .unwrap()
                .contains("individually selected")
        );
        {
            let selected = state.preview.lock().await;
            let selected = selected.as_ref().unwrap();
            assert_eq!(selected.run.jobs.len(), 1);
            assert_eq!(selected.run.jobs[0].items[0].to_string(), chosen);
        }
        let result = client
            .post(format!("{url}/api/preview"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"selection":{"targets":[],"range":{"kind":"all"}}}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            result.status(),
            400,
            "an empty selection must never expand to all messages"
        );
        let stats = Stats::new(1, 1);
        let (_, controls) = Engine::new(
            EngineConfig::default(),
            DiscordClient::with_base("", "http://127.0.0.1:1").unwrap(),
            stats.clone(),
        );
        *state.live.lock().await = Some(Live {
            id: "test-run".into(),
            stats,
            controls,
            account: "offline fixture".into(),
        });
        let busy = client
            .post(format!("{url}/api/catalog"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            busy.status(),
            400,
            "browsing must not add requests during an active deletion run"
        );
        *state.live.lock().await = None;
        let _guard = state.run_lock.lock().await;
        let result = client
            .post(format!("{url}/api/run"))
            .header("x-discord-purge-session", "test-session")
            .json(&json!({"execute":true,"preview_id":"any"}))
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 400);
        let body: serde_json::Value = result.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("being prepared"));
        server.abort();
    }

    /// Read one event of the panel's stream.
    async fn next_event(client: &reqwest::Client, url: &str) -> serde_json::Value {
        let mut events = client
            .get(format!("{url}/api/events?session=test-session"))
            .send()
            .await
            .unwrap();
        let chunk = events.chunk().await.unwrap().unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        serde_json::from_str(text.trim().strip_prefix("data: ").unwrap()).unwrap()
    }

    /// The run endpoint writes the resume log and a report, so this test runs in
    /// a child process with a private state directory.
    #[tokio::test]
    async fn the_page_can_follow_a_run_that_ends_at_once_and_a_paused_run() {
        const CHILD: &str = "DISCORD_PURGE_WEB_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let temporary = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::the_page_can_follow_a_run_that_ends_at_once_and_a_paused_run",
                ])
                .env("DISCORD_PURGE_STATE_DIR", temporary.path())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        async fn me(headers: axum::http::HeaderMap) -> axum::response::Response {
            if headers["authorization"] == "revoked" {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(json!({"id":"7","username":"owner"})).into_response()
        }
        let discord = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", discord.local_addr().unwrap());
        let discord_task = tokio::spawn(async move {
            let app = Router::new().route("/v9/users/@me", get(me));
            axum::serve(discord, app).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let state = Arc::new(AppState {
            defaults: Options {
                token: None,
                package: Some(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../../tests/fixtures/package.zip"),
                ),
                guild: None,
                channel: None,
                filter: Filter::default(),
                settings: Default::default(),
                include_pinned: false,
                token_store: resume::TokenStore::Off,
            },
            live: Mutex::new(None),
            preview: Mutex::new(None),
            run_lock: Mutex::new(()),
            session: "test-session".into(),
            host: host.clone(),
            api_base,
        });
        let app = router(state.clone());
        let panel = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .default_headers(
                [(
                    axum::http::HeaderName::from_static("x-discord-purge-session"),
                    HeaderValue::from_static("test-session"),
                )]
                .into_iter()
                .collect(),
            )
            .build()
            .unwrap();
        let url = format!("http://{host}");
        let post = |path: &'static str, body: serde_json::Value| {
            let (client, url) = (client.clone(), url.clone());
            async move {
                let response = client
                    .post(format!("{url}{path}"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                (status, response.json::<serde_json::Value>().await.unwrap())
            }
        };

        // A rejected token: the page learns it, and the preview survives.
        let (_, preview) = post("/api/preview", json!({})).await;
        let (status, rejected) = post(
            "/api/run",
            json!({"token":"revoked","execute":true,"preview_id":preview["preview_id"]}),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(rejected["token_rejected"], true);
        assert!(state.preview.lock().await.is_some());

        // Every previewed message is already in the resume log, so the run has
        // nothing to do and ends before the next event.
        let done: Vec<String> = state
            .preview
            .lock()
            .await
            .as_ref()
            .unwrap()
            .run
            .jobs
            .iter()
            .flat_map(|job| job.items.iter().map(u64::to_string))
            .collect();
        std::fs::create_dir_all(resume::state_dir()).unwrap();
        std::fs::write(resume::log_path("7"), done.join("\n")).unwrap();
        let (status, started) = post(
            "/api/run",
            json!({"token":"good","execute":true,"preview_id":preview["preview_id"]}),
        )
        .await;
        assert_eq!(status, 200, "{started}");
        assert_eq!(started["total"], 0);
        assert_eq!(started["already_done"], 50);
        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = next_event(&client, &url).await;
                if event["phase"]["kind"] == "completed" {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            event["run_id"], started["run_id"],
            "the page matches its run by this ID"
        );
        assert_eq!(event["label"], "Finished");

        // An ended run accepts no action.
        let (status, refused) = post("/api/control", json!({"action":"pause"})).await;
        assert_eq!(status, 400);
        assert!(refused["error"].as_str().unwrap().contains("has ended"));

        // A page that loads during a pause must see the pause.
        let stats = Stats::new(1, 1);
        let (_, controls) = Engine::new(
            EngineConfig::default(),
            DiscordClient::with_base("", "http://127.0.0.1:1").unwrap(),
            stats.clone(),
        );
        *state.live.lock().await = Some(Live {
            id: "active".into(),
            stats,
            controls,
            account: "fixture".into(),
        });
        let (status, _) = post("/api/control", json!({"action":"pause"})).await;
        assert_eq!(status, 200);
        let event = next_event(&client, &url).await;
        assert_eq!(event["phase"]["kind"], "paused");
        assert_eq!(event["label"], "Paused");
        panel.abort();
        discord_task.abort();
    }
}
