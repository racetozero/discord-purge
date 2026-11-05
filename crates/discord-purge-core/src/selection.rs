//! Shared read-only browsing and exact message selection for both interfaces.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::api::{BrowseMessage, Channel, CurrentUser, DiscordClient, Guild, is_deletable_type};
use crate::engine::ChannelJob;
use crate::filter::{Filter, classify, cmp_snowflake};
use crate::package::{LoadedPackage, PkgMessage};
use crate::search::{DiscoverConfig, MAX_CHANNELS, Target};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub struct MessageRef {
    pub channel_id: String,
    pub message_id: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageRange {
    #[default]
    All,
    Older {
        anchor: MessageRef,
    },
    Newer {
        anchor: MessageRef,
    },
    Only {
        messages: Vec<MessageRef>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Selection {
    pub targets: Vec<Target>,
    #[serde(default)]
    pub range: MessageRange,
    #[serde(default)]
    pub include_pinned: bool,
}

#[derive(Serialize)]
pub struct Catalog {
    pub account: CurrentUser,
    pub guilds: Vec<Guild>,
    pub dms: Vec<Channel>,
}

pub fn parse_id(id: &str) -> Option<u64> {
    (!id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
}

pub fn validate_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        parse_id(id).is_some(),
        "You entered an invalid Discord ID. Use a positive decimal server, channel, or message ID without leading zeros and retry."
    );
    Ok(())
}

impl Selection {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.targets.is_empty(),
            "You selected no channels or DMs. Select at least one target, or use Select all."
        );
        ensure!(
            self.targets.len() <= 20000,
            "You selected too many targets. Select at most 20000 targets per preview."
        );
        for target in &self.targets {
            match target {
                Target::Guild { guild_id, channel } => {
                    validate_id(guild_id)?;
                    if let Some(id) = channel {
                        validate_id(id)?;
                    }
                }
                Target::Dm { channel_id } => validate_id(channel_id)?,
            }
        }
        let refs = match &self.range {
            MessageRange::All => &[][..],
            MessageRange::Older { anchor } | MessageRange::Newer { anchor } => {
                std::slice::from_ref(anchor)
            }
            MessageRange::Only { messages } => {
                ensure!(
                    !messages.is_empty() && messages.len() <= 10000,
                    "You selected no messages or too many messages. Select 1 to 10000 messages, or use All messages."
                );
                messages.as_slice()
            }
        };
        for message in refs {
            validate_id(&message.channel_id)?;
            validate_id(&message.message_id)?;
        }
        Ok(())
    }

    pub fn contains_channel(&self, channel: &Channel) -> bool {
        self.targets.iter().any(|target| match target {
            Target::Guild {
                guild_id,
                channel: selected,
            } => {
                channel.guild_id.as_ref() == Some(guild_id)
                    && selected.as_ref().is_none_or(|id| id == &channel.id)
            }
            Target::Dm { channel_id } => matches!(channel.kind, 1 | 3) && channel.id == *channel_id,
        })
    }

    pub fn description(&self) -> String {
        let scope = format!("{} selected scope(s)", self.targets.len());
        match &self.range {
            MessageRange::All => format!("All your matching messages in {scope}"),
            MessageRange::Older { anchor } => format!(
                "Your messages older than {} in {scope}; boundary excluded",
                anchor.message_id
            ),
            MessageRange::Newer { anchor } => format!(
                "Your messages newer than {} in {scope}; boundary excluded",
                anchor.message_id
            ),
            MessageRange::Only { messages } => format!(
                "{} individually selected message(s) in {scope}",
                messages.len()
            ),
        }
    }
}

/// A package file's identity: a changed size or change time reads it again.
type PackageStamp = (PathBuf, u64, Option<SystemTime>);

/// Browsing asks for the same package for each list and page, so keep the
/// last result of each kind.
type Cache<K, T> = Mutex<Option<(K, T)>>;
static PACKAGE_IDS: Cache<PackageStamp, Arc<LoadedPackage<u64>>> = Mutex::new(None);
static PACKAGE_HISTORY: Cache<(PackageStamp, String), Arc<Vec<PkgMessage>>> = Mutex::new(None);

fn package_stamp(path: &Path) -> anyhow::Result<PackageStamp> {
    let meta = std::fs::metadata(path).with_context(|| {
        format!(
            "Cannot read the package {}. Check that the path exists and that you can read it.",
            path.display()
        )
    })?;
    Ok((path.to_path_buf(), meta.len(), meta.modified().ok()))
}

fn cached<K: PartialEq, T: Clone>(
    slot: &Cache<K, T>,
    key: K,
    load: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_key, value)) = slot.as_ref()
        && *cached_key == key
    {
        return Ok(value.clone());
    }
    let value = load()?;
    *slot = Some((key, value.clone()));
    Ok(value)
}

fn package_ids(path: &Path) -> anyhow::Result<Arc<LoadedPackage<u64>>> {
    cached(&PACKAGE_IDS, package_stamp(path)?, || {
        Ok(Arc::new(crate::package::load_ids(
            path,
            &Filter::default(),
        )?))
    })
}

fn package_channel<M>(channel: &crate::package::RawChannel<M>) -> Channel {
    Channel {
        id: channel.id.clone(),
        name: Some(if channel.is_dm || channel.name.is_empty() {
            channel.label()
        } else {
            channel.name.clone()
        }),
        kind: if channel.is_dm { 1 } else { 0 },
        guild_id: (!channel.guild_id.is_empty()).then(|| channel.guild_id.clone()),
        recipients: Vec::new(),
    }
}

pub async fn catalog(client: &DiscordClient, package: Option<&Path>) -> anyhow::Result<Catalog> {
    if let Some(path) = package {
        let package = package_ids(path)?;
        let mut guilds = BTreeMap::new();
        let mut dms = Vec::new();
        for channel in &package.channels {
            if channel.is_dm {
                dms.push(package_channel(channel));
            } else if !channel.guild_id.is_empty() {
                guilds.insert(
                    channel.guild_id.clone(),
                    Guild {
                        id: channel.guild_id.clone(),
                        name: channel.guild_name.clone(),
                    },
                );
            }
        }
        return Ok(Catalog {
            account: CurrentUser::offline("offline package"),
            guilds: guilds.into_values().collect(),
            dms,
        });
    }
    let account = client.current_user().await?;
    let (guilds, dms) = tokio::try_join!(client.guilds(), client.dms())?;
    Ok(Catalog {
        account,
        guilds,
        dms,
    })
}

pub async fn channels(
    client: &DiscordClient,
    package: Option<&Path>,
    guild_ids: &[String],
) -> anyhow::Result<Vec<Channel>> {
    for id in guild_ids {
        validate_id(id)?;
    }
    if let Some(path) = package {
        return Ok(package_ids(path)?
            .channels
            .iter()
            .filter(|c| guild_ids.iter().any(|id| id == &c.guild_id))
            .map(package_channel)
            .collect());
    }
    let mut channels = Vec::new();
    let mut seen = HashSet::new();
    for id in guild_ids {
        for channel in client.guild_channels(id).await? {
            if seen.insert(channel.id.clone()) {
                channels.push(channel);
            }
        }
    }
    Ok(channels)
}

pub async fn messages(
    client: &DiscordClient,
    package: Option<&Path>,
    channel_id: &str,
    before: Option<&str>,
) -> anyhow::Result<Vec<BrowseMessage>> {
    validate_id(channel_id)?;
    if let Some(id) = before {
        validate_id(id)?;
    }
    if let Some(path) = package {
        let key = (package_stamp(path)?, channel_id.to_string());
        let history = cached(&PACKAGE_HISTORY, key, || {
            let channel = crate::package::load_channel(path, channel_id)?
                .channels
                .into_iter()
                .next()
                .context(
                    "The channel is not in this package. Select a listed channel and retry.",
                )?;
            let mut messages = channel.messages;
            messages.sort_by(|a, b| cmp_snowflake(&b.id, &a.id));
            Ok(Arc::new(messages))
        })?;
        return Ok(history
            .iter()
            .filter(|m| before.is_none_or(|id| cmp_snowflake(&m.id, id).is_lt()))
            .take(crate::api::HISTORY_PAGE)
            .map(|m| BrowseMessage {
                id: m.id.clone(),
                channel_id: channel_id.into(),
                content: m.content.clone(),
                author: CurrentUser::offline("you"),
                pinned: false,
                kind: 0,
                attachments: Vec::new(),
            })
            .collect());
    }
    client.messages(channel_id, before).await
}

/// Build only the selected jobs. Browsing and discovery never send DELETE.
pub async fn discover(
    client: &DiscordClient,
    selection: &Selection,
    package: Option<&Path>,
    account: &str,
    filter: &Filter,
) -> anyhow::Result<Vec<ChannelJob>> {
    selection.validate()?;
    let refs: &[MessageRef] = match &selection.range {
        MessageRange::Older { anchor } | MessageRange::Newer { anchor } => {
            std::slice::from_ref(anchor)
        }
        MessageRange::Only { messages } => messages,
        MessageRange::All => &[],
    };
    // `validate` checked every ID, so these parse.
    let required: HashSet<(&str, u64)> = refs
        .iter()
        .map(|r| {
            (
                r.channel_id.as_str(),
                parse_id(&r.message_id).unwrap_or_default(),
            )
        })
        .collect();
    let keep = |channel: &str, id: u64| match &selection.range {
        MessageRange::All => true,
        MessageRange::Older { .. } => required.iter().all(|&(_, anchor)| id < anchor),
        MessageRange::Newer { .. } => required.iter().all(|&(_, anchor)| id > anchor),
        MessageRange::Only { .. } => required.contains(&(channel, id)),
    };
    if let Some(path) = package {
        let package = crate::package::scan(path, &Filter::default(), |id, content, kind| {
            (id, filter.keep_id(id, content, kind))
        })?;
        let found: HashSet<(&str, u64)> = package
            .channels
            .iter()
            .filter(|c| selection.contains_channel(&package_channel(c)))
            .flat_map(|c| c.messages.iter().map(move |&(id, _)| (c.id.as_str(), id)))
            .filter(|key| required.contains(key))
            .collect();
        ensure!(
            found.len() == required.len(),
            "A selected message is outside the selected package channels. Choose the message from the current scope and retry."
        );
        return Ok(package
            .channels
            .into_iter()
            .filter(|c| selection.contains_channel(&package_channel(c)))
            .filter_map(|c| {
                let label = c.label();
                let id = c.id;
                let items: Vec<_> = c
                    .messages
                    .into_iter()
                    .filter(|&(message, passes)| passes && keep(&id, message))
                    .map(|(message, _)| message)
                    .collect();
                (!items.is_empty()).then_some(ChannelJob {
                    channel_id: id,
                    label,
                    items,
                    verified: false,
                })
            })
            .collect());
    }
    let mut channel_cache = BTreeMap::new();
    for reference in refs {
        if !channel_cache.contains_key(&reference.channel_id) {
            let channel = client.channel(&reference.channel_id).await?;
            ensure!(
                selection.contains_channel(&channel),
                "A selected message is outside your selected servers, channels, or DMs. Select a message from the current scope and retry."
            );
            channel_cache.insert(reference.channel_id.clone(), channel);
        }
    }
    if let MessageRange::Only { messages } = &selection.range {
        let mut jobs: BTreeMap<String, ChannelJob> = BTreeMap::new();
        let mut seen = HashSet::new();
        for reference in messages {
            if !seen.insert(reference) {
                continue;
            }
            let message = client
                .message(&reference.channel_id, &reference.message_id)
                .await?;
            ensure!(
                message.id == reference.message_id && message.channel_id == reference.channel_id,
                "Discord returned a different message. Refresh the message list and preview again."
            );
            ensure!(
                message.author.id == account,
                "You selected another account's message. Select only your own messages and retry."
            );
            if !is_deletable_type(message.kind, message.pinned, selection.include_pinned) {
                continue;
            }
            let names = message.attachments.iter().map(|a| a.filename.as_str());
            if !filter.keep(
                &message.id,
                &message.content,
                classify(&message.content, names),
            ) {
                continue;
            }
            let channel = channel_cache.get(&reference.channel_id).with_context(|| {
                format!(
                    "Channel {} is not in the preview. Complete a new preview and retry.",
                    reference.channel_id
                )
            })?;
            jobs.entry(reference.channel_id.clone())
                // The preview read each selected message's author and pin state.
                .or_insert_with(|| ChannelJob {
                    channel_id: channel.id.clone(),
                    label: channel.label(),
                    items: Vec::new(),
                    verified: true,
                })
                .items
                .push(reference.message_id.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "The selected message ID {:?} is not numeric. Choose the message from the list and retry.",
                        reference.message_id
                    )
                })?);
        }
        return Ok(jobs.into_values().collect());
    }
    if let MessageRange::Older { anchor } | MessageRange::Newer { anchor } = &selection.range {
        client
            .message(&anchor.channel_id, &anchor.message_id)
            .await?;
    }
    let mut filter = filter.clone();
    match &selection.range {
        MessageRange::Older { anchor }
            if filter
                .before_id
                .as_ref()
                .is_none_or(|id| cmp_snowflake(&anchor.message_id, id).is_lt()) =>
        {
            filter.before_id = Some(anchor.message_id.clone())
        }
        MessageRange::Newer { anchor }
            if filter
                .after_id
                .as_ref()
                .is_none_or(|id| cmp_snowflake(&anchor.message_id, id).is_gt()) =>
        {
            filter.after_id = Some(anchor.message_id.clone())
        }
        _ => {}
    }
    let all_guilds: HashSet<&str> = selection
        .targets
        .iter()
        .filter_map(|t| match t {
            Target::Guild {
                guild_id,
                channel: None,
            } => Some(guild_id.as_str()),
            _ => None,
        })
        .collect();
    // One search per server covers all of its selected channels, as the
    // client's `in:` filter does. An empty set searches the whole server.
    let mut server_channels: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut dms = BTreeSet::new();
    for target in &selection.targets {
        match target {
            Target::Guild { guild_id, channel } => {
                let channels = server_channels.entry(guild_id).or_default();
                if let Some(c) = channel
                    && !all_guilds.contains(guild_id.as_str())
                {
                    channels.insert(c);
                }
            }
            Target::Dm { channel_id } => {
                dms.insert(channel_id.as_str());
            }
        }
    }
    let searches = server_channels
        .into_iter()
        .flat_map(|(guild, channels)| {
            let channels: Vec<String> = channels.into_iter().map(String::from).collect();
            if channels.is_empty() {
                vec![(Some(guild.to_string()), channels)]
            } else {
                channels
                    .chunks(MAX_CHANNELS)
                    .map(|chunk| (Some(guild.to_string()), chunk.to_vec()))
                    .collect()
            }
        })
        .chain(dms.into_iter().map(|c| (None, vec![c.to_string()])));
    let mut seen_messages = HashSet::new();
    let mut jobs: BTreeMap<String, ChannelJob> = BTreeMap::new();
    for (index, (guild_id, channel_ids)) in searches.enumerate() {
        let config = DiscoverConfig {
            author_id: account.into(),
            guild_id,
            channel_ids,
            include_pinned: selection.include_pinned,
            ..Default::default()
        };
        if index > 0 {
            // Pause between searches as between pages, so many targets do not
            // send a burst of searches.
            let pace = 0.5 + rand::random::<f64>();
            tokio::time::sleep(config.search_delay.mul_f64(pace)).await;
        }
        for mut job in crate::search::discover(client, &config, &filter).await? {
            ensure!(
                config.channel_ids.is_empty() || config.channel_ids.contains(&job.channel_id),
                "Discord returned messages outside the selected channel. Refresh the scope and preview again."
            );
            job.items
                .retain(|&m| keep(&job.channel_id, m) && seen_messages.insert(m));
            if job.items.is_empty() {
                continue;
            }
            jobs.entry(job.channel_id.clone())
                .or_insert_with(|| ChannelJob {
                    channel_id: job.channel_id.clone(),
                    label: job.label.clone(),
                    items: Vec::new(),
                    verified: true,
                })
                .items
                .extend(job.items);
        }
    }
    Ok(jobs.into_values().collect())
}
