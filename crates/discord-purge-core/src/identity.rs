//! Runtime-only identity: fresh process IDs, the public build number, and an
//! installation ID only when Discord supplies one.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use crate::client_metadata::{ClientMetadata, encode_properties};

static PROCESS: OnceCell<Arc<ProcessSession>> = OnceCell::const_new();
const CACHE_VERSION: u32 = 2;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedIdentity {
    version: u32,
    installation_id: Option<String>,
    super_properties: String,
    created_at: String,
}

pub fn cache_path() -> anyhow::Result<PathBuf> {
    let directory = match std::env::var_os("DISCORD_PURGE_IDENTITY_DIR") {
        Some(path) => PathBuf::from(path),
        None => dirs::home_dir().context("Cannot find your home directory. Set DISCORD_PURGE_IDENTITY_DIR to a private directory and retry.")?
            .join(".local/share/discord-purge"),
    };
    Ok(directory.join(".client-identity.json"))
}

/// Create fresh local process metadata without making a network request.
/// Public build metadata and installation are resolved before network use.
pub async fn initialize() -> anyhow::Result<PathBuf> {
    let metadata = ClientMetadata::detect()?;
    Ok(process_session(&metadata).await?.path.clone())
}

pub(crate) async fn process_session(
    metadata: &ClientMetadata,
) -> anyhow::Result<Arc<ProcessSession>> {
    PROCESS
        .get_or_try_init(|| async { Ok(Arc::new(ProcessSession::start(metadata, cache_path()?)?)) })
        .await
        .cloned()
}

pub(crate) struct ProcessSession {
    path: PathBuf,
    installation: Option<HeaderValue>,
    properties: Map<String, Value>,
    prepared: OnceCell<HeaderMap>,
    created_at: String,
}

impl ProcessSession {
    fn start(metadata: &ClientMetadata, path: PathBuf) -> anyhow::Result<Self> {
        let mut properties = metadata.properties.clone();
        properties.insert("client_launch_id".into(), Value::String(session_uuid()));
        properties.insert(
            "client_heartbeat_session_id".into(),
            Value::String(session_uuid()),
        );
        let installation = match std::fs::read(&path) {
            Ok(bytes) => {
                let cached: CachedIdentity = serde_json::from_slice(&bytes).with_context(|| format!("The identity cache {} is invalid. Remove this cache file and retry; keep your token and deletion progress.", path.display()))?;
                ensure!(
                    cached.version == CACHE_VERSION,
                    "The identity cache has an unsupported format. Use format version {CACHE_VERSION} or remove {} and retry.",
                    path.display()
                );
                cached
                    .installation_id
                    .as_deref()
                    .map(installation_header)
                    .transpose()?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Cannot read identity cache {}. Check file permissions and retry.",
                        path.display()
                    )
                });
            }
        };
        let session = Self {
            path,
            installation,
            properties,
            prepared: OnceCell::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let encoded = encode_properties(&session.properties);
        session.persist(&encoded, session.installation.as_ref())?;
        Ok(session)
    }

    fn persist(&self, properties: &str, installation: Option<&HeaderValue>) -> anyhow::Result<()> {
        let cached = CachedIdentity {
            version: CACHE_VERSION,
            installation_id: installation
                .map(|v| v.to_str().map(String::from))
                .transpose()?,
            super_properties: properties.to_string(),
            created_at: self.created_at.clone(),
        };
        crate::resume::write_private_file(&self.path, &serde_json::to_vec_pretty(&cached)?)
            .context("Cannot save the identity cache. Check DISCORD_PURGE_IDENTITY_DIR and file permissions before retrying.")?;
        Ok(())
    }

    pub(crate) async fn headers(
        &self,
        http: &reqwest::Client,
        base: &str,
        http3: bool,
    ) -> anyhow::Result<HeaderMap> {
        self.prepared.get_or_try_init(|| async {
            let mut public_page = reqwest::Url::parse(base)?;
            public_page.set_path("/app"); public_page.set_query(None);
            let mut request = http.get(public_page);
            if http3 { request = request.version(reqwest::Version::HTTP_3); }
            let mut response = request.send().await.context("Cannot fetch public PTB build metadata. Check your connection and UDP access, then retry.")?;
            ensure!(response.status().is_success(), "PTB build metadata returned HTTP {}. Retry when the public app page is available; no account request was sent.", response.status());
            let mut html = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(html.len() + chunk.len() <= 1024 * 1024, "The public PTB page is too large. Retry after the page format is supported; no account request was sent.");
                html.extend_from_slice(&chunk);
            }
            let html = std::str::from_utf8(&html).context("PTB returned invalid build metadata. Retry later; no account request was sent.")?;
            let pattern = regex::Regex::new(r#""BUILD_NUMBER"\s*:\s*"([0-9]+)""#)?;
            let build: u64 = pattern.captures(html).and_then(|c| c.get(1)).context("The public PTB page has no build number. Update the app's metadata parser and retry; no account request was sent.")?.as_str().parse()?;
            ensure!(build > 0, "PTB returned an invalid build number. Retry later; no account request was sent.");
            let mut properties = self.properties.clone();
            properties.insert("client_build_number".into(), Value::from(build));
            let encoded = encode_properties(&properties);
            let mut properties = HeaderValue::from_str(&encoded)?; properties.set_sensitive(true);
            let installation = if let Some(value) = &self.installation { Some(value.clone()) } else {
                #[derive(Deserialize)] struct Experiments { installation: Option<String> }
                let mut installation = None;
                for attempt in 0..3 {
                    let mut request = http.get(format!("{base}/{}/experiments", crate::api::API_VERSION))
                        .query(&[("with_guild_experiments", "true")]).header("X-Super-Properties", properties.clone());
                    if http3 { request = request.version(reqwest::Version::HTTP_3); }
                    let response = request.send().await.context("Cannot obtain an installation ID from PTB. Check your connection and UDP access, then retry.")?;
                    let status = response.status();
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let header = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
                        let body: Value = response.json().await.unwrap_or(Value::Null);
                        let seconds = body.get("retry_after").and_then(Value::as_f64).or(header).unwrap_or(1.0);
                        ensure!(seconds.is_finite() && seconds > 0.0 && seconds <= 86400.0, "PTB returned an unsupported bootstrap wait. Retry later; no account request was sent.");
                        ensure!(attempt < 2, "PTB still limits installation requests. Wait at least {seconds} seconds before retrying; no account request was sent.");
                        tokio::time::sleep(Duration::from_secs_f64(seconds)).await; continue;
                    }
                    ensure!(status.is_success(), "PTB installation bootstrap returned HTTP {status}. Retry when the public experiments route is available; no account request was sent.");
                    let body: Experiments = response.json().await.context("PTB returned invalid installation data. Retry later; no account request was sent.")?;
                    installation = body.installation.as_deref().map(installation_header).transpose()?;
                    break;
                }
                installation
            };
            self.persist(&encoded, installation.as_ref())?;
            let mut headers = HeaderMap::new();
            headers.insert("x-super-properties", properties);
            if let Some(value) = installation { headers.insert("x-installation-id", value); }
            Ok(headers)
        }).await.cloned()
    }
}

fn installation_header(value: &str) -> anyhow::Result<HeaderValue> {
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "The installation value is empty or too long. Remove the identity cache and retry the PTB bootstrap."
    );
    let mut value = HeaderValue::from_str(value).context("The installation value is not a valid HTTP header. Remove the identity cache and retry the PTB bootstrap.")?;
    value.set_sensitive(true);
    Ok(value)
}

fn session_uuid() -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = u128::from_be_bytes(bytes);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        value >> 96,
        (value >> 80) & 0xffff,
        (value >> 64) & 0xffff,
        (value >> 48) & 0xffff,
        value & 0xffffffffffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn bootstrap_is_shared_and_server_installation_survives_restart() {
        let metadata = ClientMetadata::detect().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/.client-identity.json");
        let session = ProcessSession::start(&metadata, path.clone()).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        async fn issue(State(count): State<Arc<AtomicUsize>>) -> Json<Value> {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Json(serde_json::json!({"installation":"server-installation"}))
        }
        let app = Router::new()
            .route("/app", get(|| async { "<script>window.GLOBAL_ENV={\"BUILD_NUMBER\":\"123456\",\"RELEASE_CHANNEL\":\"ptb\"}</script>" }))
            .route("/v9/experiments", get(issue))
            .route("/without-installation/app", get(|| async { "<script>window.GLOBAL_ENV={\"BUILD_NUMBER\":\"123456\"}</script>" }))
            .route(
                "/without-installation/v9/experiments",
                get(|| async { Json(serde_json::json!({"fingerprint":"not-an-installation"})) }),
            )
            .with_state(count.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        let (a, b) = tokio::join!(
            session.headers(&http, &base, false),
            session.headers(&http, &base, false)
        );
        let a = a.unwrap();
        assert_eq!(a, b.unwrap());
        assert_eq!(a["x-installation-id"], "server-installation");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let restarted = ProcessSession::start(&metadata, path.clone()).unwrap();
        let headers = restarted.headers(&http, &base, false).await.unwrap();
        assert_eq!(headers["x-installation-id"], a["x-installation-id"]);
        assert_ne!(headers["x-super-properties"], a["x-super-properties"]);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let absent_path = directory.path().join("optional.json");
        let absent = ProcessSession::start(&metadata, absent_path.clone()).unwrap();
        let absent_headers = absent
            .headers(&http, &format!("{base}/without-installation"), false)
            .await
            .unwrap();
        assert!(!absent_headers.contains_key("x-installation-id"));
        let cached: Value = serde_json::from_slice(&std::fs::read(absent_path).unwrap()).unwrap();
        assert!(cached["installation_id"].is_null());
        server.abort();
    }

    #[tokio::test]
    async fn invalid_bootstrap_is_not_cached_and_rate_limit_retries_are_bounded() {
        let metadata = ClientMetadata::detect().unwrap();
        for (status, body, attempts) in [
            (200, "{\"installation\":\"\"}", 1),
            (200, "not json", 1),
            (503, "{}", 1),
            (429, "{\"retry_after\":0.001}", 3),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("identity.json");
            let session = ProcessSession::start(&metadata, path.clone()).unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let calls = count.clone();
            let app = Router::new()
                .route(
                    "/app",
                    get(|| async {
                        "<script>window.GLOBAL_ENV={\"BUILD_NUMBER\":\"123456\"}</script>"
                    }),
                )
                .fallback(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move { (axum::http::StatusCode::from_u16(status).unwrap(), body) }
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            assert!(
                session
                    .headers(&reqwest::Client::new(), &base, false)
                    .await
                    .is_err()
            );
            assert_eq!(count.load(Ordering::SeqCst), attempts);
            let cache: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert!(cache["installation_id"].is_null());
            server.abort();
        }
    }
    #[tokio::test]
    async fn invalid_public_metadata_stops_before_installation_bootstrap() {
        let metadata = ClientMetadata::detect().unwrap();
        for (status, body) in [
            (503, b"unavailable".to_vec()),
            (200, b"<html>no build metadata</html>".to_vec()),
            (200, br#"{"BUILD_NUMBER":"0"}"#.to_vec()),
            (200, br#"{"BUILD_NUMBER":"123invalid"}"#.to_vec()),
            (200, vec![0xff]),
            (200, vec![b'x'; 1024 * 1024 + 1]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let session =
                ProcessSession::start(&metadata, directory.path().join("identity.json")).unwrap();
            let bootstrap_calls = Arc::new(AtomicUsize::new(0));
            let calls = bootstrap_calls.clone();
            let app = Router::new()
                .route(
                    "/app",
                    get(move || {
                        let body = body.clone();
                        async move { (axum::http::StatusCode::from_u16(status).unwrap(), body) }
                    }),
                )
                .fallback(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Json(serde_json::json!({"installation":"must-not-be-used"})) }
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            assert!(
                session
                    .headers(&reqwest::Client::new(), &base, false)
                    .await
                    .is_err()
            );
            assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }
}
