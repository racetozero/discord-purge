//! Discord PTB REST client with generated runtime headers.
//! Account tokens remain separate from the profile. HTTPS requests require
//! HTTP/3. Local HTTP endpoints are used by the offline server tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// The API version this tool targets. v9 is the version the web client uses and
/// used by some reference tools. Search is an unsupported user endpoint.
pub const API_VERSION: &str = "v9";

pub const DEFAULT_BASE: &str = "https://ptb.discord.com/api";
/// The messages in one page of channel history. A full page means more can follow.
pub const HISTORY_PAGE: usize = 50;
/// The account ID of a preview that no account has confirmed yet.
pub(crate) const UNKNOWN_ACCOUNT: &str = "unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Per-route, per-user bucket. Parallel channels still help here.
    User,
    Shared,
    Global,
}

impl Scope {
    pub fn from_headers(scope: Option<&str>, global: bool) -> Scope {
        if global {
            return Scope::Global;
        }
        match scope {
            Some("shared") => Scope::Shared,
            Some("global") => Scope::Global,
            _ => Scope::User,
        }
    }

    pub fn should_reduce_concurrency(self) -> bool {
        matches!(self, Scope::Shared | Scope::Global)
    }
}

#[derive(Debug, Clone)]
pub enum DeleteOutcome {
    Deleted,
    /// 404: the message was already gone. For bulk deletion this is success.
    AlreadyGone,
    /// 403 on a system message: undeletable by anyone, forever. The string is
    /// Discord's reason. Record it and never ask again.
    SystemForbidden(String),
    /// 403 for any other reason (usually lost access to the guild/DM). The
    /// string is Discord's reason. Rejoining makes it deletable, so retry later.
    LostAccess(String),
    RateLimited {
        retry_after: Duration,
        scope: Scope,
    },
    Unauthorized,
    Fatal(String),
    Permanent(String),
    Protected(String),
    Transient(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CurrentUser {
    pub id: String,
    #[serde(default)]
    pub username: String,
    pub global_name: Option<String>,
    pub discriminator: Option<String>,
}

impl CurrentUser {
    /// The placeholder account of a package, which does not name its account.
    pub(crate) fn offline(username: &str) -> Self {
        Self {
            id: UNKNOWN_ACCOUNT.into(),
            username: username.into(),
            global_name: None,
            discriminator: None,
        }
    }
}

impl CurrentUser {
    pub fn label(&self) -> String {
        let handle = match &self.discriminator {
            Some(d) if d != "0" && !d.is_empty() => format!("{}#{}", self.username, d),
            _ => format!("@{}", self.username),
        };
        match &self.global_name {
            Some(n) if !n.is_empty() => format!("{n} ({handle})"),
            _ => handle,
        }
    }
}

/// One message returned by a search. Discord sends `messages` as an array of
/// arrays; only the element with `hit == true` is the actual match.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(rename = "type")]
    pub kind: i64,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hit: bool,
    pub author: CurrentUser,
    #[serde(default)]
    pub attachments: Vec<BrowseAttachment>,
}

#[derive(Debug, Clone)]
pub struct SearchPage {
    pub messages: Vec<SearchMessage>,
    /// Discord's reported total (decreases as you delete). May be approximate.
    pub total_results: i64,
    /// `Some(wait)` when Discord returned 202 (index still building).
    pub indexing: Option<Duration>,
}

/// Parameters for a message search. Either `guild_id` (server, optionally
/// limited to `channel_ids`) or exactly one channel ID (DM) must be set.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub guild_id: Option<String>,
    pub channel_ids: Vec<String>,
    pub author_id: String,
    pub min_id: Option<String>,
    pub max_id: Option<String>,
    /// Results to skip. Discord accepts 0 to 9975.
    pub offset: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Channel {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: u8,
    pub guild_id: Option<String>,
    #[serde(default)]
    pub recipients: Vec<CurrentUser>,
}

impl Channel {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let names = self
                    .recipients
                    .iter()
                    .map(CurrentUser::label)
                    .collect::<Vec<_>>()
                    .join(", ");
                if names.is_empty() {
                    format!("channel {}", self.id)
                } else {
                    names
                }
            })
    }
}

/// A message displayed in the scope browser. It is never a deletion receipt.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowseMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub content: String,
    pub author: CurrentUser,
    pub pinned: bool,
    #[serde(default, rename = "type")]
    pub kind: i64,
    #[serde(default)]
    pub attachments: Vec<BrowseAttachment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowseAttachment {
    pub filename: String,
}

#[derive(Debug, Default, Deserialize)]
struct ApiError {
    retry_after: Option<f64>,
    #[serde(default)]
    global: bool,
    code: Option<i64>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSearch {
    #[serde(default)]
    messages: Vec<Vec<SearchMessage>>,
    #[serde(default)]
    total_results: i64,
}

/// Discord's error code for acting on a system message. These are undeletable
/// forever, so we record them and skip them permanently.
pub const ERR_SYSTEM_MESSAGE: i64 = 50021;

#[derive(Default)]
struct Limits {
    /// The account-wide floor: no request starts before this instant.
    next: Option<tokio::time::Instant>,
    /// The end of a global 429. Every route waits for it.
    global_until: Option<tokio::time::Instant>,
    /// Route template -> the `x-ratelimit-bucket` hash Discord gave it.
    /// Several routes can share one bucket.
    buckets: HashMap<String, String>,
    /// Bucket hash (or route template, before the hash is known) plus the
    /// major ID -> requests left and when the bucket refills.
    gates: HashMap<String, (u32, tokio::time::Instant)>,
}

impl Limits {
    fn gate_key(&self, route: &str, major: &str) -> String {
        format!(
            "{}:{major}",
            self.buckets.get(route).map_or(route, String::as_str)
        )
    }
}

/// Split a request into Discord's rate-limit identity: the route template, with
/// IDs replaced, and the major parameter (the channel, guild, or webhook ID).
fn route_key(method: &reqwest::Method, path: &str) -> (String, String) {
    let mut major = String::new();
    let mut previous = "";
    let template: Vec<&str> = path
        .split('/')
        .map(|segment| {
            let id = !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
            let result = match (id, previous) {
                (true, "channels" | "guilds" | "webhooks") if major.is_empty() => {
                    major = segment.to_string();
                    "{major}"
                }
                (true, _) => "{id}",
                _ => segment,
            };
            previous = segment;
            result
        })
        .collect();
    (format!("{method} {}", template.join("/")), major)
}

#[derive(Clone)]
pub struct DiscordClient {
    http: reqwest::Client,
    base: String,
    token: String,
    account_id: Arc<OnceLock<String>>,
    include_pinned: bool,
    limits: Arc<Mutex<Limits>>,
    http3: bool,
    metadata: Option<Arc<crate::client_metadata::ClientMetadata>>,
}

impl DiscordClient {
    pub fn new(token: impl Into<String>) -> anyhow::Result<Self> {
        Self::with_base(token, DEFAULT_BASE)
    }

    pub fn with_base(token: impl Into<String>, base: impl Into<String>) -> anyhow::Result<Self> {
        let base = base.into();
        let endpoint = reqwest::Url::parse(&base).map_err(|_| {
            anyhow::anyhow!(
            "The API URL is invalid. Use https://ptb.discord.com/api or a local test server URL.")
        })?;
        let local = endpoint.host_str() == Some("localhost")
            || endpoint
                .host_str()
                .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
        anyhow::ensure!(
            (endpoint.scheme() == "https" && endpoint.host_str() == Some("ptb.discord.com"))
                || (local && matches!(endpoint.scheme(), "http" | "https")),
            "The API host is not supported. Use https://ptb.discord.com/api or a loopback test server."
        );
        let http3 = endpoint.scheme() == "https";
        let metadata = if local {
            None
        } else {
            Some(crate::client_metadata::ClientMetadata::current()?)
        };
        let headers = match metadata.as_ref() {
            Some(metadata) => metadata.header_map()?,
            None => reqwest::header::HeaderMap::new(),
        };
        let http = client_builder(headers, http3).build()?;
        Ok(Self {
            http,
            base,
            token: token.into(),
            account_id: Arc::new(OnceLock::new()),
            include_pinned: false,
            limits: Arc::default(),
            http3,
            metadata,
        })
    }

    async fn read<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<T> {
        anyhow::ensure!(
            !self.token.trim().is_empty(),
            "You did not supply a token. Enter your account token to browse servers, DMs, and messages."
        );
        let mut attempt = 0;
        loop {
            let response = self
                .send(
                    self.http
                        .get(self.url(path))
                        .query(query)
                        .header("Authorization", &self.token),
                )
                .await?;
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let header = header_f64(response.headers(), "retry-after");
                let body: ApiError = response.json().await.unwrap_or_default();
                let wait = wait_secs(body.retry_after.or(header), 1.0);
                anyhow::ensure!(
                    attempt < 2,
                    "Discord still limits browsing. Wait at least {} seconds, then retry. No message was deleted.",
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }
            anyhow::ensure!(
                status.is_success(),
                "Discord returned HTTP {status} while browsing. Check your token and channel access, then retry. No message was deleted."
            );
            return response.json().await.context("Discord returned invalid browse data. Retry after the API is available. No message was deleted.");
        }
    }

    pub async fn guilds(&self) -> anyhow::Result<Vec<Guild>> {
        let mut guilds = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut query = vec![("limit", "200".into())];
            if let Some(id) = &after {
                query.push(("after", id.clone()));
            }
            let page: Vec<Guild> = self.read("users/@me/guilds", &query).await?;
            let count = page.len();
            let next = page
                .iter()
                .map(|g| &g.id)
                .max_by(|a, b| crate::filter::cmp_snowflake(a, b))
                .cloned();
            if let Some(next) = next {
                anyhow::ensure!(
                    after
                        .as_ref()
                        .is_none_or(|a| crate::filter::cmp_snowflake(&next, a).is_gt()),
                    "Discord repeated the server list. Retry browsing before you select servers."
                );
                after = Some(next);
            }
            guilds.extend(page);
            anyhow::ensure!(
                guilds.len() <= 20000,
                "The server list is too large. Use a specific server ID or a data package."
            );
            if count < 200 {
                break;
            }
        }
        guilds.sort_by_cached_key(|g| g.name.to_lowercase());
        let mut seen = std::collections::HashSet::new();
        guilds.retain(|g| seen.insert(g.id.clone()));
        Ok(guilds)
    }

    pub async fn guild_channels(&self, guild: &str) -> anyhow::Result<Vec<Channel>> {
        crate::selection::validate_id(guild)?;
        let mut channels: Vec<Channel> =
            self.read(&format!("guilds/{guild}/channels"), &[]).await?;
        for channel in &mut channels {
            channel.guild_id = Some(guild.to_string());
        }
        channels.retain(|c| matches!(c.kind, 0 | 2 | 5 | 10 | 11 | 12 | 13 | 15 | 16));
        channels.sort_by_cached_key(Channel::label);
        Ok(channels)
    }

    pub async fn dms(&self) -> anyhow::Result<Vec<Channel>> {
        let mut channels: Vec<Channel> = self.read("users/@me/channels", &[]).await?;
        channels.retain(|c| matches!(c.kind, 1 | 3));
        channels.sort_by_cached_key(Channel::label);
        Ok(channels)
    }

    pub async fn channel(&self, id: &str) -> anyhow::Result<Channel> {
        crate::selection::validate_id(id)?;
        self.read(&format!("channels/{id}"), &[]).await
    }

    pub async fn messages(
        &self,
        channel: &str,
        before: Option<&str>,
    ) -> anyhow::Result<Vec<BrowseMessage>> {
        crate::selection::validate_id(channel)?;
        let mut query = vec![("limit", HISTORY_PAGE.to_string())];
        if let Some(id) = before {
            crate::selection::validate_id(id)?;
            query.push(("before", id.into()));
        }
        self.read(&format!("channels/{channel}/messages"), &query)
            .await
    }

    pub async fn message(&self, channel: &str, message: &str) -> anyhow::Result<BrowseMessage> {
        crate::selection::validate_id(channel)?;
        crate::selection::validate_id(message)?;
        let messages: Vec<BrowseMessage> = self
            .read(
                &format!("channels/{channel}/messages"),
                &[("around", message.into()), ("limit", "1".into())],
            )
            .await?;
        messages.into_iter().find(|m|m.id==message && m.channel_id==channel).context("The selected message was not returned by channel history. Refresh the message list and check Read Message History permission before you retry.")
    }

    pub fn include_pinned(mut self, include: bool) -> Self {
        self.include_pinned = include;
        self
    }

    // Gate every HTTP request, including metadata checks, the way Discord asks:
    // an account-wide spacing floor, a pause for a global 429, and a separate
    // gate for each bucket and major ID. One exhausted channel bucket does not
    // hold requests for other channels or routes.
    async fn send(&self, request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let session = match &self.metadata {
            Some(metadata) => Some(crate::identity::process_session(metadata).await?),
            None if !self.http3 => None,
            None => anyhow::bail!(
                "Client metadata is unavailable. Check your system locale and timezone, then restart the app."
            ),
        };
        let request = if let Some(session) = session {
            request.headers(session.headers(&self.http, &self.base, self.http3).await?)
        } else {
            request
        };
        let request = if self.http3 {
            request.version(reqwest::Version::HTTP_3)
        } else {
            request
        };
        let request = request.build()?;
        let (route, major) = route_key(request.method(), request.url().path());
        loop {
            let until = {
                let mut limits = self
                    .limits
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = tokio::time::Instant::now();
                let key = limits.gate_key(&route, &major);
                let gate = limits.gates.get(&key).copied();
                let until = [limits.next, limits.global_until]
                    .into_iter()
                    .flatten()
                    .chain(gate.filter(|&(left, _)| left == 0).map(|(_, reset)| reset))
                    .max()
                    .filter(|&until| until > now);
                if until.is_none() {
                    limits.next = Some(now + Duration::from_millis(40));
                    // Count this request against the bucket until its reply
                    // reports the real number left.
                    match gate {
                        Some((_, reset)) if reset <= now => {
                            limits.gates.remove(&key);
                        }
                        Some((left, reset)) => {
                            limits.gates.insert(key, (left.saturating_sub(1), reset));
                        }
                        None => {}
                    }
                }
                until
            };
            match until {
                Some(until) => tokio::time::sleep_until(until).await,
                None => break,
            }
        }
        let response = self.http.execute(request).await?;
        let headers = response.headers();
        let now = tokio::time::Instant::now();
        let mut limits = self
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(bucket) = header_str(headers, "x-ratelimit-bucket") {
            limits.buckets.insert(route.clone(), bucket);
        }
        let key = limits.gate_key(&route, &major);
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = wait_secs(
                header_f64(headers, "retry-after")
                    .or_else(|| header_f64(headers, "x-ratelimit-reset-after")),
                1.0,
            );
            let global = headers.contains_key("x-ratelimit-global")
                || header_str(headers, "x-ratelimit-scope").as_deref() == Some("global");
            if global {
                limits.global_until = limits.global_until.max(Some(now + wait));
            } else {
                limits.gates.insert(key, (0, now + wait));
            }
        } else if let Some(left) = header_f64(headers, "x-ratelimit-remaining") {
            let reset = wait_secs(header_f64(headers, "x-ratelimit-reset-after"), 1.0);
            limits
                .gates
                .insert(key, (left.max(0.0) as u32, now + reset));
        }
        drop(limits);
        Ok(response)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}/{}", self.base, API_VERSION, path)
    }

    /// Check the public PTB gateway route without sending an account token.
    pub async fn check_connection(&self) -> anyhow::Result<()> {
        let response = self.send(self.http.get(self.url("gateway"))).await
            .context("The PTB HTTP/3 check failed. Check DNS, UDP access, and system settings before you retry.")?;
        anyhow::ensure!(
            response.status().is_success(),
            "The PTB transport check returned HTTP {}. Retry when the PTB API is available.",
            response.status()
        );
        anyhow::ensure!(
            response.version() == reqwest::Version::HTTP_3,
            "The transport check did not use HTTP/3. Build with the repository Cargo configuration and retry."
        );
        Ok(())
    }

    pub async fn current_user(&self) -> anyhow::Result<CurrentUser> {
        anyhow::ensure!(
            !self.token.trim().is_empty(),
            "You did not supply a token. Set DISCORD_TOKEN or use login to supply your account token."
        );
        let response = self
            .send(
                self.http
                    .get(self.url("users/@me"))
                    .header("Authorization", &self.token),
            )
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("the token was rejected (401). Check that it is a current user token.");
        }
        if !response.status().is_success() {
            anyhow::bail!("GET /users/@me returned {}", response.status());
        }
        let me: CurrentUser = response.json().await?;
        self.account_id.get_or_init(|| me.id.clone());
        Ok(me)
    }

    /// When `verified` is false, a history check first confirms the author and
    /// pin state; discovery that already read them from Discord passes true.
    pub async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
        verified: bool,
    ) -> DeleteOutcome {
        if self.account_id.get().is_none() && self.current_user().await.is_err() {
            return DeleteOutcome::Unauthorized;
        }
        let url = self.url(&format!("channels/{channel_id}/messages/{message_id}"));
        if !verified && let Some(outcome) = self.check_history(channel_id, message_id).await {
            return outcome;
        }
        let response = match self
            .send(
                self.http
                    .delete(&url)
                    .header("Authorization", &self.token)
                    // The desktop client sends Origin only on requests that change data.
                    .header("Origin", "https://ptb.discord.com")
                    .header(
                        "Referer",
                        format!("https://ptb.discord.com/channels/@me/{channel_id}"),
                    ),
            )
            .await
        {
            Ok(r) => r,
            Err(error) => return DeleteOutcome::Transient(error.to_string()),
        };

        Self::delete_outcome(response).await
    }

    /// Confirm through channel history that the message is ours and may be
    /// deleted. `None` means send the DELETE; `Some` is the final outcome, and
    /// no DELETE is sent.
    async fn check_history(&self, channel_id: &str, message_id: &str) -> Option<DeleteOutcome> {
        let inspection = match self
            .send(
                self.http
                    .get(self.url(&format!("channels/{channel_id}/messages")))
                    .query(&[("around", message_id), ("limit", "1")])
                    .header("Authorization", &self.token),
            )
            .await
        {
            Ok(r) => r,
            Err(error) => {
                return Some(DeleteOutcome::Transient(format!(
                    "Message check failed: {error}. Check your connection and retry."
                )));
            }
        };
        if inspection.status().is_success() {
            let messages = match inspection.json::<Vec<BrowseMessage>>().await {
                Ok(messages) => messages,
                Err(_) => return Some(DeleteOutcome::Permanent("The history check returned no valid owner or pin state. Refresh the message list and retry; no DELETE was sent.".into())),
            };
            // History centred on an ID always holds that message while it
            // exists, and a missing permission answers 403 before this point.
            let Some(info) = messages
                .into_iter()
                .find(|m| m.id == message_id && m.channel_id == channel_id)
            else {
                return Some(DeleteOutcome::AlreadyGone);
            };
            if Some(&info.author.id) != self.account_id.get() {
                return Some(DeleteOutcome::Protected("The message belongs to another account. Only your own messages can be deleted.".into()));
            }
            if info.pinned && !self.include_pinned {
                return Some(DeleteOutcome::Protected(
                    "The message is pinned. Use --include-pinned to include pinned messages."
                        .into(),
                ));
            }
            None
        } else {
            Some(Self::delete_outcome(inspection).await)
        }
    }

    async fn delete_outcome(response: reqwest::Response) -> DeleteOutcome {
        let status = response.status().as_u16();
        let scope_header = header_str(response.headers(), "x-ratelimit-scope");
        let global_header = response.headers().contains_key("x-ratelimit-global");
        let retry_header = header_f64(response.headers(), "retry-after")
            .or_else(|| header_f64(response.headers(), "x-ratelimit-reset-after"));
        let body = response.text().await.unwrap_or_default();
        let error: ApiError = serde_json::from_str(&body).unwrap_or_default();

        match status {
            204 | 200 => DeleteOutcome::Deleted,
            404 if error.code == Some(10008) => DeleteOutcome::AlreadyGone,
            404 => DeleteOutcome::LostAccess(
                "The channel is not available. Check access before you retry.".into(),
            ),
            429 => DeleteOutcome::RateLimited {
                retry_after: wait_secs(error.retry_after.or(retry_header), 1.0),
                scope: Scope::from_headers(scope_header.as_deref(), error.global || global_header),
            },
            403 if error.code == Some(20002) => DeleteOutcome::Fatal("Discord rejected an app endpoint: Only bots can use this endpoint. Update the app before retrying; the run was stopped.".into()),
            403 => {
                let reason = error.message.unwrap_or_else(|| "Forbidden".into());
                if error.code == Some(ERR_SYSTEM_MESSAGE) {
                    DeleteOutcome::SystemForbidden(reason)
                } else {
                    DeleteOutcome::LostAccess(reason)
                }
            }
            400 if error.code == Some(50083) => DeleteOutcome::LostAccess(
                "The thread is archived. Restore thread access before you retry.".into(),
            ),
            401 => DeleteOutcome::Unauthorized,
            s if (400..500).contains(&s) => DeleteOutcome::Permanent(format!(
                "HTTP {s}: {}. Check message access and retry after you correct the problem.",
                snippet(&body)
            )),
            s if s >= 500 => DeleteOutcome::Transient(format!("server {s}")),
            s => DeleteOutcome::Transient(format!("unexpected HTTP {s}: {}", snippet(&body))),
        }
    }

    pub async fn search(&self, params: &SearchParams) -> anyhow::Result<SearchPage> {
        let base_path = match (&params.guild_id, params.channel_ids.as_slice()) {
            (Some(g), _) => format!("guilds/{g}/messages/search"),
            (None, [c]) => format!("channels/{c}/messages/search"),
            (None, _) => anyhow::bail!(
                "A search needs a server ID, or exactly one DM channel ID. Supply one of them and retry."
            ),
        };

        // Filters come before sorting and paging, in the desktop client's order.
        let mut query: Vec<(&str, String)> = vec![("author_id", params.author_id.clone())];
        if params.guild_id.is_some() {
            query.extend(params.channel_ids.iter().map(|c| ("channel_id", c.clone())));
        }
        if let Some(m) = &params.min_id {
            query.push(("min_id", m.clone()));
        }
        if let Some(m) = &params.max_id {
            query.push(("max_id", m.clone()));
        }
        query.push(("sort_by", "timestamp".into()));
        query.push(("sort_order", "desc".into()));
        query.push(("offset", params.offset.to_string()));
        query.push(("include_nsfw", "true".into()));

        let mut request = self
            .http
            .get(self.url(&base_path))
            .query(&query)
            .header("Authorization", &self.token);
        if let Some(channel) = params.channel_ids.first() {
            let guild = params.guild_id.as_deref().unwrap_or("@me");
            request = request.header(
                "Referer",
                format!("https://ptb.discord.com/channels/{guild}/{channel}"),
            );
        }
        let response = self.send(request).await?;

        let status = response.status().as_u16();
        let retry_header = header_f64(response.headers(), "retry-after");
        let body = response.text().await.unwrap_or_default();

        // 202 means the index is still building; 429 means slow down. Both
        // mean: wait, then ask for the same page again.
        if status == 202 || status == 429 {
            let error: ApiError = serde_json::from_str(&body).unwrap_or_default();
            let fallback = if status == 202 { 1.0 } else { 5.0 };
            return Ok(SearchPage {
                messages: vec![],
                total_results: 0,
                indexing: Some(wait_secs(error.retry_after.or(retry_header), fallback)),
            });
        }
        if status == 401 {
            anyhow::bail!("search returned 401: the token is invalid or expired.");
        }
        if !(200..300).contains(&status) {
            anyhow::bail!("search returned HTTP {status}: {}", snippet(&body));
        }

        let raw: RawSearch = serde_json::from_str(&body)
            .map_err(|error| anyhow::anyhow!("could not parse search response: {error}"))?;
        let messages = raw
            .messages
            .into_iter()
            .filter_map(|group| {
                group
                    .iter()
                    .find(|m| m.hit && m.author.id == params.author_id)
                    .cloned()
            })
            .collect();
        Ok(SearchPage {
            messages,
            total_results: raw.total_results,
            indexing: None,
        })
    }
}

pub(crate) fn client_builder(
    headers: reqwest::header::HeaderMap,
    http3: bool,
) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30));
    if http3 {
        builder.http3_prior_knowledge()
    } else {
        builder
    }
}

pub(crate) fn wait_secs(seconds: Option<f64>, fallback: f64) -> Duration {
    Duration::from_secs_f64(
        seconds
            .filter(|n| n.is_finite() && *n > 0.0 && *n <= 86400.0)
            .unwrap_or(fallback),
    )
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

fn header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    header_str(headers, name).and_then(|s| s.parse().ok())
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 120 {
        let cutoff: String = s.chars().take(120).collect();
        format!("{cutoff}…")
    } else {
        s.to_string()
    }
}

/// A user is allowed to delete type 0 (normal), replies, and most notification
/// types, but not the system types the data package and search both carry.
/// Non-deletable system types, from Discord's message-type table.
pub fn is_deletable_type(kind: i64, pinned: bool, include_pinned: bool) -> bool {
    const NON_DELETABLE: &[i64] = &[1, 2, 3, 4, 5, 21, 35, 64];
    !NON_DELETABLE.contains(&kind) && (!pinned || include_pinned)
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        response::IntoResponse,
    };
    use base64::Engine as _;
    use serde_json::json;
    use std::sync::Mutex;
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    #[tokio::test]
    async fn generated_headers_reach_every_route() {
        // The process identity is global, so isolate its cache in a child.
        if std::env::var_os("IDENTITY_DELIVERY_CHILD").is_none() {
            let state = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "api::identity_tests::generated_headers_reach_every_route",
                    "--nocapture",
                ])
                .env("IDENTITY_DELIVERY_CHILD", "1")
                .env("DISCORD_PURGE_STATE_DIR", state.path())
                .env("DISCORD_PURGE_IDENTITY_DIR", state.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let metadata = crate::client_metadata::ClientMetadata::detect().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        async fn record(State(seen): State<Seen>, request: Request<Body>) -> impl IntoResponse {
            let (parts, body) = request.into_parts();
            assert_eq!(
                parts.headers["x-discord-timezone"].to_str().unwrap(),
                iana_time_zone::get_timezone().unwrap()
            );
            assert_eq!(
                parts.headers.get("origin").map(|v| v.to_str().unwrap()),
                (parts.method == reqwest::Method::DELETE).then_some("https://ptb.discord.com")
            );
            assert_eq!(parts.headers["accept-encoding"], "gzip, deflate, br, zstd");
            let user_agent = parts.headers["user-agent"].to_str().unwrap();
            assert!(user_agent.contains(" discord/") && user_agent.contains(" Electron/"));
            assert!(!parts.headers.contains_key("cookie"));
            assert!(to_bytes(body, 1024).await.unwrap().is_empty());
            let path = parts.uri.path().to_string();
            seen.lock()
                .unwrap()
                .push((parts.method.to_string(), path.clone()));
            if path == "/app" {
                assert!(!parts.headers.contains_key("authorization"));
                assert!(!parts.headers.contains_key("x-installation-id"));
                assert!(!parts.headers.contains_key("x-super-properties"));
                return "<script>window.GLOBAL_ENV={\"BUILD_NUMBER\":\"123456\"}</script>"
                    .into_response();
            }
            let encoded = base64::engine::general_purpose::STANDARD
                .decode(parts.headers["x-super-properties"].as_bytes())
                .unwrap();
            // The desktop client writes these fields in a fixed order.
            let text = String::from_utf8_lossy(&encoded);
            let order: Vec<_> = [
                "\"os\"",
                "\"browser\"",
                "\"client_version\"",
                "\"client_build_number\"",
                "\"client_app_state\"",
            ]
            .iter()
            .map(|key| text.find(key).unwrap())
            .collect();
            assert!(order.is_sorted(), "{text}");
            let properties: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(properties["client_build_number"], 123456);
            assert_eq!(properties["browser"], "Discord Client");
            assert_eq!(properties["browser_user_agent"], user_agent);
            assert!(matches!(
                properties["os_arch"].as_str(),
                Some("arm64" | "x64")
            ));
            assert!(properties.get("launch_signature").is_none());
            if path.ends_with("/experiments") {
                assert!(!parts.headers.contains_key("authorization"));
                assert!(!parts.headers.contains_key("x-installation-id"));
                assert_eq!(parts.uri.query(), Some("with_guild_experiments=true"));
                return Json(json!({"installation":"server-issued-installation"})).into_response();
            }
            assert_eq!(
                parts.headers["x-installation-id"],
                "server-issued-installation"
            );
            assert_eq!(parts.headers["authorization"], "current-account-token");
            if path.ends_with("/search") {
                assert_eq!(
                    parts.headers["referer"],
                    "https://ptb.discord.com/channels/@me/10"
                );
                assert!(
                    parts
                        .uri
                        .query()
                        .unwrap()
                        .split('&')
                        .any(|params| params == "include_nsfw=true")
                );
                return Json(json!({"messages":[],"total_results":0})).into_response();
            }
            if path.ends_with("/@me") {
                return Json(json!({"id":"7"})).into_response();
            }
            if path.ends_with("/messages") {
                assert!(parts.uri.query().unwrap().contains("around=20"));
                assert!(parts.uri.query().unwrap().contains("limit=1"));
                return Json(
                    json!([{"id":"20","channel_id":"10","author":{"id":"7"},"pinned":false}]),
                )
                .into_response();
            }
            if parts.method == reqwest::Method::DELETE {
                assert_eq!(
                    parts.headers["referer"],
                    "https://ptb.discord.com/channels/@me/10"
                );
                return axum::http::StatusCode::NO_CONTENT.into_response();
            }
            Json(json!({"author":{"id":"7"},"pinned":false})).into_response()
        }
        let app = Router::new().fallback(record).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut client = DiscordClient::with_base("current-account-token", base).unwrap();
        client.http = client_builder(metadata.header_map().unwrap(), false)
            .build()
            .unwrap();
        client.metadata = Some(Arc::new(metadata));
        assert!(matches!(
            client.delete_message("10", "20", false).await,
            DeleteOutcome::Deleted
        ));
        client
            .search(&SearchParams {
                channel_ids: vec!["10".into()],
                author_id: "7".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("GET".into(), "/app".into()),
                ("GET".into(), "/v9/experiments".into()),
                ("GET".into(), "/v9/users/@me".into()),
                ("GET".into(), "/v9/channels/10/messages".into()),
                ("DELETE".into(), "/v9/channels/10/messages/20".into()),
                ("GET".into(), "/v9/channels/10/messages/search".into())
            ]
        );
        server.abort();
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use axum::{Router, extract::Path, response::IntoResponse, routing::get};
    use std::time::Instant;

    #[test]
    fn route_key_keeps_only_the_major_id() {
        assert_eq!(
            route_key(&reqwest::Method::DELETE, "/api/v9/channels/12/messages/34"),
            (
                "DELETE /api/v9/channels/{major}/messages/{id}".to_string(),
                "12".to_string()
            )
        );
        assert_eq!(
            route_key(&reqwest::Method::GET, "/api/v9/users/@me/guilds").1,
            ""
        );
    }

    #[tokio::test]
    async fn an_empty_bucket_holds_only_its_own_channel() {
        // Channel 1's bucket is empty for 400 ms; channel 2's bucket is not.
        async fn history(Path(channel): Path<String>) -> impl IntoResponse {
            let left = if channel == "1" { "0" } else { "4" };
            (
                [
                    ("x-ratelimit-bucket", "history"),
                    ("x-ratelimit-remaining", left),
                    ("x-ratelimit-reset-after", "0.4"),
                ],
                "[]",
            )
        }
        let app = Router::new().route("/v9/channels/{channel}/messages", get(history));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = DiscordClient::with_base("token", base).unwrap();
        client.messages("1", None).await.unwrap();
        let start = Instant::now();
        client.messages("2", None).await.unwrap();
        assert!(start.elapsed() < Duration::from_millis(300));
        client.messages("1", None).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(300));
        server.abort();
    }
}
