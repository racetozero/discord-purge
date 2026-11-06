//! discord-purge: delete your own Discord messages.
//!
//! The library exposes the deletion engine and the two discovery sources (data
//! package and live search) so the three front ends — headless CLI, terminal
//! UI, and local web panel — all drive the same, single implementation of the
//! rate-limit logic.
//!
//! Scope: this deletes only the token owner's own messages (Discord's API
//! enforces that) and sends runtime PTB client metadata.

pub mod api;
pub(crate) mod client_metadata;
pub mod engine;
pub mod filter;
pub mod identity;
pub mod package;
pub mod resume;
pub mod search;
pub mod selection;

use std::path::PathBuf;

use engine::{ChannelJob, EngineConfig};

/// How to find your Discord user token. Shown by `discord-purge login` and in
/// the TUI token screen, so both front ends tell the same story.
pub const TOKEN_HELP: &str = "\
How to find your token (use Discord in a web browser, NOT the desktop app):
  1. Open https://ptb.discord.com/app in your browser and log in.
  2. Press F12 to open developer tools, then click the Network tab.
  3. Back in Discord, click any channel so requests appear in the list.
  4. Click a request to /api/ (the one named \"messages\" works well).
  5. Open its Request Headers and copy the value of the \"authorization\"
     header. That whole value is your token.

Treat the token like a password. Anyone who has it controls your account.
Never share it or paste it into a site you do not trust.";

#[derive(Debug, Clone)]
pub enum Source {
    Package(PathBuf),
    Search(search::DiscoverConfig),
    Selected {
        selection: selection::Selection,
        package: Option<PathBuf>,
    },
}

impl Source {
    pub fn from_inputs(
        package: Option<PathBuf>,
        guild: Option<String>,
        channel: Option<String>,
        include_pinned: bool,
    ) -> anyhow::Result<Self> {
        let (guild_id, channel_ids) = match (package, guild, channel) {
            (Some(path), None, None) => {
                anyhow::ensure!(
                    path.exists(),
                    "The package path {} does not exist. Use the path of the Discord data package ZIP or its extracted folder.",
                    path.display()
                );
                return Ok(Source::Package(path));
            }
            (Some(_), _, _) => anyhow::bail!(
                "You combined a package with live search. Use a package, or use a server and/or channel ID."
            ),
            (None, Some(guild_id), channel) => (Some(guild_id), channel.into_iter().collect()),
            (None, None, Some(channel_id)) => (None, vec![channel_id]),
            (None, None, None) => anyhow::bail!(
                "You gave no source. Use a package path, a server ID (optionally with a channel ID), or a DM channel ID."
            ),
        };
        Ok(Source::Search(search::DiscoverConfig {
            guild_id,
            channel_ids,
            include_pinned,
            ..Default::default()
        }))
    }
}

pub struct Options {
    pub token: Option<String>,
    pub package: Option<PathBuf>,
    pub guild: Option<String>,
    pub channel: Option<String>,
    pub filter: filter::Filter,
    pub settings: EngineConfig,
    pub include_pinned: bool,
    /// The token shared with the other front ends. A token typed in the front
    /// end wins, then `token` (from `--token`), then the saved token.
    pub token_store: resume::TokenStore,
}

/// A preview: what a run would delete. Only `confirm` can turn it into a run.
#[derive(Clone)]
pub struct PreparedRun {
    pub account_id: String,
    pub account_label: String,
    pub jobs: Vec<ChannelJob>,
    pub total: usize,
    /// False when a search stopped before it reached the end of its results.
    /// `search::INCOMPLETE_NOTICE` tells the user what this means.
    pub complete: bool,
}

/// A preview checked against the signed-in account, with saved progress
/// removed. `Engine::run` accepts only this type, and `jobs` is private, so
/// nothing can start a deletion without the check.
pub struct ConfirmedRun {
    pub account_id: String,
    pub account_label: String,
    pub total: usize,
    pub already_done: usize,
    pub(crate) jobs: Vec<ChannelJob>,
}

impl PreparedRun {
    pub async fn confirm(&self, client: &api::DiscordClient) -> anyhow::Result<ConfirmedRun> {
        let me = client.current_user().await?;
        anyhow::ensure!(
            self.account_id == api::UNKNOWN_ACCOUNT || self.account_id == me.id,
            "You changed the account after preview. Complete a new preview for this account."
        );
        let done = resume::load_done(&me.id);
        let mut jobs = self.jobs.clone();
        let mut already_done = 0;
        for job in &mut jobs {
            let count = job.items.len();
            job.items.retain(|id| !done.contains(id));
            already_done += count - job.items.len();
        }
        jobs.retain(|job| !job.items.is_empty());
        Ok(ConfirmedRun {
            account_id: me.id.clone(),
            account_label: me.label(),
            total: jobs.iter().map(|job| job.items.len()).sum(),
            already_done,
            jobs,
        })
    }
}

/// Build the preview jobs: verify the token (except for an offline package),
/// load the source, apply the filter, and attach a per-channel label. Saved
/// progress is removed later, by `PreparedRun::confirm`.
pub async fn prepare(
    client: &api::DiscordClient,
    source: &Source,
    filter: &filter::Filter,
) -> anyhow::Result<PreparedRun> {
    let offline = matches!(
        source,
        Source::Package(_)
            | Source::Selected {
                package: Some(_),
                ..
            }
    );
    let me = if offline {
        api::CurrentUser::offline("offline preview")
    } else {
        client.current_user().await?
    };

    let search::Discovered { jobs, complete } = match source {
        Source::Package(path) => search::Discovered {
            jobs: package::load_ids(path, filter)?
                .channels
                .into_iter()
                .map(|c| ChannelJob {
                    label: c.label(),
                    channel_id: c.id,
                    items: c.messages,
                    verified: false,
                })
                .collect(),
            complete: true,
        },
        Source::Selected { selection, package } => {
            selection::discover(client, selection, package.as_deref(), &me.id, filter).await?
        }
        Source::Search(config) => {
            // Search only the signed-in user's messages, so a run never
            // touches anyone else's.
            let mut config = config.clone();
            config.author_id = me.id.clone();
            search::discover(client, &config, filter).await?
        }
    };

    let total = jobs.iter().map(|j| j.items.len()).sum();
    Ok(PreparedRun {
        account_id: me.id.clone(),
        account_label: me.label(),
        jobs,
        total,
        complete,
    })
}
