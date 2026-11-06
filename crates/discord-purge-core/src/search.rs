//! Live-search discovery.
//!
//! When the user has no data package, this finds their messages through
//! Discord's authenticated search API (the approach of `undiscord` and
//! `detcord`). It walks pages newest-first by `offset` in steps of 25, as the
//! desktop client does, and pauses about one second between pages. Past
//! Discord's 9,975 offset limit it continues below the oldest ID seen with
//! `max_id`. It keeps only the real `hit` of each result group, drops system
//! messages, applies the same offline filters as the package path, and groups
//! the survivors into `ChannelJob`s the engine can run.
//!
//! Discord's search index lags live data: a fresh target can answer 202
//! ("indexing, retry after N"), and pages can hold fewer than 25 results or
//! none, so the walk ends at `total_results`, not at a short page. We wait out
//! 202s and 429s for the time Discord states and stop after a few empty pages
//! in a row.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::Context;

use crate::api::{DiscordClient, SearchParams, is_deletable_type};
use crate::engine::ChannelJob;
use crate::filter::{Filter, classify};
use crate::selection::validate_id;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Guild {
        guild_id: String,
        channel: Option<String>,
    },
    Dm {
        channel_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    pub author_id: String,
    /// The server to search. `None` searches the one DM in `channel_ids`.
    pub guild_id: Option<String>,
    /// In a server, the channels to search (empty: all of them). For a DM,
    /// exactly one channel.
    pub channel_ids: Vec<String>,
    pub include_pinned: bool,
    /// Typical pause between search pages. Each pause is random, from half to
    /// one and a half times this value, so the cadence is not machine-regular.
    pub search_delay: Duration,
}

impl Default for DiscoverConfig {
    fn default() -> Self {
        Self {
            author_id: String::new(),
            guild_id: None,
            channel_ids: Vec::new(),
            include_pinned: false,
            search_delay: Duration::from_secs(1),
        }
    }
}

/// The jobs that a discovery found. `complete` is false when a search stopped on
/// empty pages before it reached Discord's reported total, so more messages can
/// exist.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub jobs: Vec<ChannelJob>,
    pub complete: bool,
}

/// What a front end shows when a preview is not complete.
pub const INCOMPLETE_NOTICE: &str = "Discord's search stopped early, so this count can be too low. Preview again later to find the remaining messages.";

/// How many empty pages in a row to tolerate (stale index) before stopping.
const MAX_EMPTY_PAGES: u32 = 5;
const EMPTY_BACKOFF: f64 = 1.3;
/// Discord's page size. The client advances `offset` by this even on a short page.
const PAGE_SIZE: usize = 25;
/// The largest `offset` Discord accepts.
const MAX_OFFSET: usize = 9975;
/// The most `channel_id` values Discord accepts in one server search.
pub const MAX_CHANNELS: usize = 500;

/// Walk the target and return the deletable, filtered messages as per-channel
/// jobs. It is read-only against Discord: it sends only GET search requests.
pub async fn discover(
    client: &DiscordClient,
    config: &DiscoverConfig,
    filter: &Filter,
) -> anyhow::Result<Discovered> {
    anyhow::ensure!(
        config.guild_id.is_some() || config.channel_ids.len() == 1,
        "A DM search needs exactly one channel ID, but {} were given. Search one DM at a time, or add a server ID.",
        config.channel_ids.len()
    );
    anyhow::ensure!(
        config.channel_ids.len() <= MAX_CHANNELS,
        "You selected {} channels in one server search. Use at most {MAX_CHANNELS} channels per search.",
        config.channel_ids.len()
    );
    for id in std::iter::once(&config.author_id)
        .chain(&config.guild_id)
        .chain(&config.channel_ids)
    {
        validate_id(id)?;
    }

    let mut by_channel: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut cursor: Option<String> = filter.before_id.clone(); // exclusive upper boundary
    let mut offset = 0usize;
    let mut oldest: Option<String> = None;
    let mut empty_pages = 0u32;
    let mut seen = HashSet::new();
    let mut wait_count = 0usize;

    let complete = loop {
        let params = SearchParams {
            guild_id: config.guild_id.clone(),
            channel_ids: config.channel_ids.clone(),
            author_id: config.author_id.clone(),
            min_id: filter.after_id.clone(),
            max_id: cursor.clone(),
            offset,
        };
        let page = client.search(&params).await?;

        if let Some(wait) = page.indexing {
            // 202 (indexing) or a search 429: wait and retry the same page.
            wait_count += 1;
            anyhow::ensure!(
                wait_count <= 12,
                "Search did not become available after 12 waits. Retry later or use a data package."
            );
            tokio::time::sleep(wait).await;
            continue;
        }
        wait_count = 0;
        empty_pages = if page.messages.is_empty() {
            empty_pages + 1
        } else {
            0
        };

        // Each page must be strictly older than the last one, or paging is stuck.
        if let Some(page_oldest) = page
            .messages
            .iter()
            .map(|message| &message.id)
            .min_by(|a, b| crate::filter::cmp_snowflake(a, b))
        {
            anyhow::ensure!(
                oldest
                    .as_ref()
                    .is_none_or(|o| crate::filter::cmp_snowflake(page_oldest, o).is_lt()),
                "Search repeated a page. Retry later or use a data package to avoid an incomplete result."
            );
            oldest = Some(page_oldest.clone());
        }
        for message in &page.messages {
            if !is_deletable_type(message.kind, message.pinned, config.include_pinned) {
                continue;
            }
            if !seen.insert(message.id.clone()) {
                continue;
            }
            // Text, type, and date filters are local, so they work the same as
            // for a package. Discord search token matching would be different.
            let kind = classify(
                &message.content,
                message.attachments.iter().map(|a| a.filename.as_str()),
            );
            if !filter.keep(&message.id, &message.content, kind) {
                continue;
            }
            by_channel
                .entry(message.channel_id.clone())
                .or_default()
                .push(message.id.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "Discord returned an invalid message ID {:?}. Retry later or use a data package.",
                        message.id
                    )
                })?);
        }

        offset += PAGE_SIZE;
        if offset as i64 >= page.total_results {
            break true;
        }
        if empty_pages >= MAX_EMPTY_PAGES {
            break false;
        }
        if offset > MAX_OFFSET {
            // Discord max_id is exclusive: continue from the oldest ID itself so
            // an adjacent numeric ID is not skipped.
            cursor = Some(oldest.clone().context(
                "Search reached Discord's offset limit with no message ID to continue from. Narrow the search with a date range or use a data package.",
            )?);
            offset = 0;
        }
        let pace = (0.5 + rand::random::<f64>()) * EMPTY_BACKOFF.powi(empty_pages as i32);
        tokio::time::sleep(config.search_delay.mul_f64(pace)).await;
    };

    let jobs = by_channel
        .into_iter()
        .map(|(channel_id, items)| ChannelJob {
            label: format!("channel {channel_id}"),
            channel_id,
            items,
            verified: true,
        })
        .collect();
    Ok(Discovered { jobs, complete })
}
