//! Workspace state, background tasks, and key handling for the terminal UI.
//!
//! One screen holds three things the user decides: where (servers, channels,
//! DMs), which messages (all, older or newer than a boundary, or marked ones),
//! and how (filters and speed). A preview counts the exact IDs; only a
//! confirmed preview is deleted.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::KeyCode;
use discord_purge_core::api::{BrowseMessage, Channel, DiscordClient, is_deletable_type};
use discord_purge_core::engine::{Controls, Engine, EngineConfig, Stats};
use discord_purge_core::filter::Filter;
use discord_purge_core::search::Target;
use discord_purge_core::selection::{self, Catalog, MessageRange, MessageRef, Selection, parse_id};
use discord_purge_core::{Options, PreparedRun, Source, prepare, resume};
use tokio::task::{JoinHandle, JoinSet};

pub(crate) const TYPES: [&str; 7] = discord_purge_core::filter::TYPE_NAMES;

pub(crate) struct Server {
    pub id: String,
    pub name: String,
    /// The whole server, including threads that are not listed.
    pub all: bool,
    pub picked: BTreeSet<String>,
    pub expanded: bool,
    pub channels: Option<Vec<Channel>>,
    pub loading: bool,
}

pub(crate) struct Dm {
    pub channel: Channel,
    pub checked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Row {
    ServersHeader,
    Server(usize),
    Channel(usize, usize),
    DmsHeader,
    Dm(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Focus {
    Where,
    Messages,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Screen {
    Workspace,
    Running,
    Done,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Range {
    All,
    Older(MessageRef),
    Newer(MessageRef),
    Only(BTreeSet<MessageRef>),
}

pub(crate) struct History {
    pub channel: Channel,
    pub messages: Vec<BrowseMessage>,
    pub more: bool,
    pub loading: bool,
    pub cursor: usize,
}

pub(crate) struct SourceForm {
    pub package_mode: bool,
    pub token: String,
    pub package: String,
    /// 0: source kind, 1: token, 2: package path.
    pub field: usize,
    /// Asked for a token to delete an offline preview; keep the lists.
    pub for_delete: bool,
    pub error: Option<String>,
}

pub(crate) struct FilterForm {
    /// 0 content, 1 types, 2 start date, 3 end date, 4 pinned, 5 workers,
    /// 6 delay, 7 jitter.
    pub field: usize,
    pub content: String,
    pub types: [bool; 7],
    pub type_cursor: usize,
    pub after: String,
    pub before: String,
    pub pinned: bool,
    pub workers: String,
    pub delay: String,
    pub jitter: String,
    pub error: Option<String>,
}

pub(crate) enum Modal {
    Source(SourceForm),
    Filters(FilterForm),
    Preview {
        cursor: usize,
    },
    Confirm,
    Link {
        input: String,
        error: Option<String>,
    },
    LinkRange(MessageRef),
    Help,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Link {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: Option<String>,
}

pub(crate) fn parse_link(input: &str) -> anyhow::Result<Link> {
    let input = input.trim();
    let bad = || {
        anyhow::anyhow!(
            "You entered {input:?}, which is not a Discord link or channel ID. Paste a link from Discord's Copy Link, such as https://discord.com/channels/@me/123, or a DM channel ID."
        )
    };
    if parse_id(input).is_some() {
        return Ok(Link {
            guild_id: None,
            channel_id: input.into(),
            message_id: None,
        });
    }
    let rest = input.split_once("://").map_or(input, |(_, rest)| rest);
    let (host, path) = rest.split_once('/').ok_or_else(bad)?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if ![
        "discord.com",
        "ptb.discord.com",
        "canary.discord.com",
        "discordapp.com",
    ]
    .contains(&host)
    {
        return Err(bad());
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let ["channels", guild, channel, message @ ..] = parts.as_slice() else {
        return Err(bad());
    };
    let message = match message {
        [] => None,
        [id] => Some(id.to_string()),
        _ => return Err(bad()),
    };
    let guild = (*guild != "@me").then(|| guild.to_string());
    for id in guild
        .iter()
        .chain([channel.to_string()].iter())
        .chain(message.iter())
    {
        parse_id(id).ok_or_else(bad)?;
    }
    Ok(Link {
        guild_id: guild,
        channel_id: channel.to_string(),
        message_id: message,
    })
}

fn edit(text: &mut String, code: KeyCode) -> bool {
    match code {
        KeyCode::Char(c) if !c.is_control() => text.push(c),
        KeyCode::Backspace => {
            text.pop();
        }
        _ => return false,
    }
    true
}

pub(crate) fn move_cursor(cursor: &mut usize, last: usize, code: KeyCode) -> bool {
    *cursor = match code {
        KeyCode::Up => cursor.saturating_sub(1),
        KeyCode::Down => (*cursor + 1).min(last),
        KeyCode::PageUp => cursor.saturating_sub(10),
        KeyCode::PageDown => (*cursor + 10).min(last),
        KeyCode::Home => 0,
        KeyCode::End => last,
        _ => return false,
    };
    true
}

pub(crate) fn message_ref(message: &BrowseMessage) -> MessageRef {
    MessageRef {
        channel_id: message.channel_id.clone(),
        message_id: message.id.clone(),
    }
}

/// A finished background task. Each result names what it belongs to, so a
/// result for an old source or an old selection is dropped.
pub(crate) enum Loaded {
    Catalog {
        generation: u64,
        result: anyhow::Result<(Catalog, DiscordClient)>,
    },
    Channels {
        generation: u64,
        guild_id: String,
        result: anyhow::Result<Vec<Channel>>,
    },
    Messages {
        generation: u64,
        channel_id: String,
        older: bool,
        result: anyhow::Result<Vec<BrowseMessage>>,
    },
    Preview {
        revision: u64,
        result: anyhow::Result<PreparedRun>,
    },
    Confirmed {
        result: anyhow::Result<(PreparedRun, DiscordClient)>,
    },
}

pub(crate) struct App {
    pub config: Options,
    pub token: Option<String>,
    pub package: Option<PathBuf>,
    generation: u64,
    pub listed: bool,
    pub account: String,
    pub account_id: String,
    pub servers: Vec<Server>,
    pub dms: Vec<Dm>,
    pub query: String,
    pub typing_query: bool,
    pub cursor: usize,
    pub focus: Focus,
    pub history: Option<History>,
    pub range: Range,
    pub revision: u64,
    pub preview: Option<(PreparedRun, u64)>,
    pub modal: Option<Modal>,
    pub screen: Screen,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub activity: String,
    loads: JoinSet<Loaded>,
    job: Option<JoinHandle<Loaded>>,
    pub stats: Option<Arc<Stats>>,
    pub controls: Option<Controls>,
    pub report: String,
    pub should_quit: bool,
    pub frame: usize,
}

impl App {
    pub fn new(config: Options) -> Self {
        // `--token` wins over the token saved by the CLI, TUI, or web panel.
        let token = config
            .token
            .clone()
            .filter(|server| !server.trim().is_empty())
            .or_else(|| config.token_store.load());
        let package = config.package.clone();
        Self {
            config,
            token,
            package,
            generation: 0,
            listed: false,
            account: String::new(),
            account_id: String::new(),
            servers: Vec::new(),
            dms: Vec::new(),
            query: String::new(),
            typing_query: false,
            cursor: 0,
            focus: Focus::Where,
            history: None,
            range: Range::All,
            revision: 0,
            preview: None,
            modal: None,
            screen: Screen::Workspace,
            notice: None,
            error: None,
            activity: String::new(),
            loads: JoinSet::new(),
            job: None,
            stats: None,
            controls: None,
            report: String::new(),
            should_quit: false,
            frame: 0,
        }
    }

    pub fn start(&mut self) {
        if self.package.is_some() || self.token.is_some() {
            self.load_catalog();
        } else {
            self.open_source(false);
        }
    }

    pub fn busy(&self) -> bool {
        !self.loads.is_empty() || self.job.is_some()
    }

    pub fn preview_ready(&self) -> Option<&PreparedRun> {
        self.preview
            .as_ref()
            .filter(|(_, revision)| *revision == self.revision)
            .map(|(run, _)| run)
    }

    pub fn cancel_tasks(&mut self) {
        self.loads.abort_all();
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }

    fn changed(&mut self) {
        self.revision += 1;
    }

    fn new_client(&self) -> anyhow::Result<DiscordClient> {
        Ok(DiscordClient::new(self.token.clone().unwrap_or_default())?
            .include_pinned(self.config.include_pinned))
    }

    fn open_source(&mut self, for_delete: bool) {
        self.modal = Some(Modal::Source(SourceForm {
            package_mode: self.package.is_some() && !for_delete,
            token: String::new(),
            package: self
                .package
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            field: if for_delete || self.package.is_none() {
                1
            } else {
                2
            },
            for_delete,
            error: None,
        }));
    }

    fn open_filters(&mut self) {
        let inputs = self.config.filter.inputs();
        let chosen: Vec<&str> = inputs.types.split(',').collect();
        let settings = &self.config.settings;
        self.modal = Some(Modal::Filters(FilterForm {
            field: 0,
            content: inputs.content,
            types: TYPES.map(|t| chosen.contains(&t)),
            type_cursor: 0,
            after: inputs.after_date,
            before: inputs.before_date,
            pinned: self.config.include_pinned,
            workers: settings.workers.to_string(),
            delay: settings.delay.as_secs_f64().to_string(),
            jitter: settings.jitter.to_string(),
            error: None,
        }));
    }

    pub fn load_catalog(&mut self) {
        self.cancel_tasks();
        self.generation += 1;
        self.listed = false;
        self.servers.clear();
        self.dms.clear();
        self.history = None;
        self.range = Range::All;
        self.preview = None;
        self.cursor = 0;
        self.focus = Focus::Where;
        self.changed();
        // Pick up a token that another front end saved while this one ran.
        if self.token.is_none() {
            self.token = self.config.token_store.load();
        }
        let Some(client) = self.client_or_error() else {
            return;
        };
        let generation = self.generation;
        self.activity = "Loading servers and DMs".into();
        self.loads.spawn({
            let package = self.package.clone();
            async move {
                let result = selection::catalog(&client, package.as_deref())
                    .await
                    .map(|catalog| (catalog, client));
                Loaded::Catalog { generation, result }
            }
        });
    }

    fn client_or_error(&mut self) -> Option<DiscordClient> {
        match self.new_client() {
            Ok(client) => Some(client),
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        }
    }

    fn load_channels(&mut self, index: usize) {
        let Some(client) = self.client_or_error() else {
            return;
        };
        let server = &mut self.servers[index];
        if server.loading || server.channels.is_some() {
            return;
        }
        server.loading = true;
        let generation = self.generation;
        self.activity = format!("Loading channels of {}", server.name);
        self.loads.spawn({
            let guild_id = server.id.clone();
            let package = self.package.clone();
            async move {
                let result = selection::channels(
                    &client,
                    package.as_deref(),
                    std::slice::from_ref(&guild_id),
                )
                .await;
                Loaded::Channels {
                    generation,
                    guild_id,
                    result,
                }
            }
        });
    }

    fn load_messages(&mut self, older: bool) {
        let Some(client) = self.client_or_error() else {
            return;
        };
        let Some(history) = &mut self.history else {
            return;
        };
        if history.loading || (older && !history.more) {
            return;
        }
        history.loading = true;
        let channel_id = history.channel.id.clone();
        let before = older
            .then(|| history.messages.last().map(|message| message.id.clone()))
            .flatten();
        let package = self.package.clone();
        let generation = self.generation;
        self.activity = format!("Loading messages of {}", history.channel.label());
        self.loads.spawn(async move {
            let result =
                selection::messages(&client, package.as_deref(), &channel_id, before.as_deref())
                    .await;
            Loaded::Messages {
                generation,
                channel_id,
                older,
                result,
            }
        });
    }

    fn open_history(&mut self, channel: Channel) {
        self.focus = Focus::Messages;
        if self
            .history
            .as_ref()
            .is_some_and(|history| history.channel.id == channel.id)
        {
            return;
        }
        self.history = Some(History {
            channel,
            messages: Vec::new(),
            more: false,
            loading: false,
            cursor: 0,
        });
        self.load_messages(false);
    }

    pub fn selection(&self) -> Selection {
        let mut targets = Vec::new();
        for server in &self.servers {
            if server.all {
                targets.push(Target::Guild {
                    guild_id: server.id.clone(),
                    channel: None,
                });
            } else {
                targets.extend(server.picked.iter().map(|c| Target::Guild {
                    guild_id: server.id.clone(),
                    channel: Some(c.clone()),
                }));
            }
        }
        targets.extend(
            self.dms
                .iter()
                .filter(|dm| dm.checked)
                .map(|dm| Target::Dm {
                    channel_id: dm.channel.id.clone(),
                }),
        );
        let range = match &self.range {
            Range::All => MessageRange::All,
            Range::Older(anchor) => MessageRange::Older {
                anchor: anchor.clone(),
            },
            Range::Newer(anchor) => MessageRange::Newer {
                anchor: anchor.clone(),
            },
            Range::Only(set) => MessageRange::Only {
                messages: set.iter().cloned().collect(),
            },
        };
        Selection {
            targets,
            range,
            include_pinned: self.config.include_pinned,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let query = self.query.to_lowercase();
        let hit = |s: &str| query.is_empty() || s.to_lowercase().contains(&query);
        let mut rows = vec![Row::ServersHeader];
        for (index, server) in self.servers.iter().enumerate() {
            let channels = server.channels.as_deref().unwrap_or_default();
            let name_hit = hit(&server.name);
            let shown: Vec<usize> = (0..channels.len())
                .filter(|&channel_index| name_hit || hit(&channels[channel_index].label()))
                .collect();
            if !name_hit && shown.is_empty() {
                continue;
            }
            rows.push(Row::Server(index));
            if server.expanded {
                rows.extend(
                    shown
                        .into_iter()
                        .map(|channel_index| Row::Channel(index, channel_index)),
                );
            }
        }
        rows.push(Row::DmsHeader);
        rows.extend(
            (0..self.dms.len())
                .filter(|&index| hit(&self.dms[index].channel.label()))
                .map(Row::Dm),
        );
        rows
    }

    pub fn own(&self, message: &BrowseMessage) -> bool {
        message.author.id == self.account_id
            && is_deletable_type(message.kind, message.pinned, self.config.include_pinned)
    }

    /// Whether the scope covers the open channel. One draw calls this once,
    /// because a large selection is slow to rebuild for each message.
    pub fn history_in_scope(&self) -> bool {
        self.history
            .as_ref()
            .is_some_and(|history| self.selection().contains_channel(&history.channel))
    }

    pub fn in_range(&self, message: &BrowseMessage, history_in_scope: bool) -> bool {
        if !history_in_scope || !self.own(message) {
            return false;
        }
        let id = parse_id(&message.id).unwrap_or_default();
        match &self.range {
            Range::All => true,
            Range::Older(a) => id < parse_id(&a.message_id).unwrap_or_default(),
            Range::Newer(a) => id > parse_id(&a.message_id).unwrap_or_default(),
            Range::Only(set) => set.contains(&message_ref(message)),
        }
    }

    fn ensure_in_scope(&mut self, channel: &Channel) {
        if self.selection().contains_channel(channel) {
            return;
        }
        match &channel.guild_id {
            Some(guild_id) => {
                let index = match self
                    .servers
                    .iter()
                    .position(|server| &server.id == guild_id)
                {
                    Some(i) => i,
                    None => {
                        self.servers.push(Server {
                            id: guild_id.clone(),
                            name: format!("Server {guild_id}"),
                            all: false,
                            picked: BTreeSet::new(),
                            expanded: false,
                            channels: None,
                            loading: false,
                        });
                        self.servers.len() - 1
                    }
                };
                self.servers[index].picked.insert(channel.id.clone());
            }
            None => match self.dms.iter_mut().find(|d| d.channel.id == channel.id) {
                Some(dm) => dm.checked = true,
                None => self.dms.push(Dm {
                    channel: channel.clone(),
                    checked: true,
                }),
            },
        }
        self.notice = Some(format!("Added {} to the scope.", channel.label()));
    }

    fn set_range(&mut self, range: Range, channel: &Channel) {
        self.ensure_in_scope(channel);
        self.range = range;
        self.changed();
    }

    fn start_preview(&mut self) {
        if self.job.is_some() {
            self.notice = Some(
                "A preview or start check is running. Wait, or press Esc to cancel it.".into(),
            );
            return;
        }
        let selection = self.selection();
        if let Err(error) = selection.validate() {
            self.error = Some(error.to_string());
            return;
        }
        let Some(client) = self.client_or_error() else {
            return;
        };
        let revision = self.revision;
        self.activity = "Counting the exact messages. Nothing is deleted".into();
        self.job = Some(tokio::spawn({
            let source = Source::Selected {
                selection,
                package: self.package.clone(),
            };
            let filter = self.config.filter.clone();
            async move {
                let result = prepare(&client, &source, &filter).await;
                Loaded::Preview { revision, result }
            }
        }));
    }

    fn ask_delete(&mut self) {
        match self.preview_ready() {
            None => self.start_preview(),
            Some(run) if run.total == 0 => {
                self.notice = Some("The preview found no messages. Change the scope, range, or filters and preview again.".into());
                self.modal = None;
            }
            Some(_) if self.token.is_none() => self.open_source(true),
            Some(_) => self.modal = Some(Modal::Confirm),
        }
    }

    fn start_run(&mut self) {
        let Some(preview) = self.preview_ready().cloned() else {
            self.error = Some(
                "The preview is out of date. Press p to preview again before you delete.".into(),
            );
            return;
        };
        let Some(client) = self.client_or_error() else {
            return;
        };
        self.activity = "Checking the account and saved progress".into();
        self.job = Some(tokio::spawn(async move {
            let result = preview.confirm(&client).await.map(|run| (run, client));
            Loaded::Confirmed { result }
        }));
    }

    fn begin_run(&mut self, run: PreparedRun, client: DiscordClient) -> anyhow::Result<()> {
        let log = resume::ResumeLog::open(&run.account_id)?;
        let stats = Stats::new(run.total, self.config.settings.workers);
        let (engine, controls) = Engine::new(self.config.settings.clone(), client, stats.clone());
        if run.already_done > 0 {
            self.notice = Some(format!(
                "Skipped {} message(s) that an earlier run already deleted.",
                run.already_done
            ));
        }
        self.account = run.account_label;
        self.stats = Some(stats);
        self.controls = Some(controls);
        tokio::spawn(async move {
            engine.run(run.jobs, move |key| log.record(key)).await;
        });
        self.screen = Screen::Running;
        Ok(())
    }

    fn apply(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Catalog { generation, .. }
            | Loaded::Channels { generation, .. }
            | Loaded::Messages { generation, .. }
                if generation != self.generation => {}
            Loaded::Catalog { result, .. } => match result {
                Ok((catalog, _)) => {
                    self.listed = true;
                    self.account = if self.package.is_some() {
                        "offline package".into()
                    } else {
                        catalog.account.label()
                    };
                    self.account_id = catalog.account.id.clone();
                    let guild = self.config.guild.take();
                    let channel = self.config.channel.take();
                    self.servers = catalog
                        .guilds
                        .into_iter()
                        .map(|g| {
                            let preset = guild.as_ref() == Some(&g.id);
                            Server {
                                all: preset && channel.is_none(),
                                picked: channel.iter().filter(|_| preset).cloned().collect(),
                                id: g.id,
                                name: g.name,
                                expanded: false,
                                channels: None,
                                loading: false,
                            }
                        })
                        .collect();
                    self.dms = catalog
                        .dms
                        .into_iter()
                        .map(|c| Dm {
                            checked: guild.is_none() && channel.as_ref() == Some(&c.id),
                            channel: c,
                        })
                        .collect();
                    if let (None, Some(id)) = (&guild, &channel)
                        && !self.dms.iter().any(|dm| &dm.channel.id == id)
                    {
                        self.dms.push(Dm {
                            channel: Channel {
                                id: id.clone(),
                                name: Some(format!("DM {id}")),
                                kind: 1,
                                guild_id: None,
                                recipients: Vec::new(),
                            },
                            checked: true,
                        });
                    }
                    self.notice = Some(format!(
                        "Loaded {} server(s) and {} DM(s). Space selects; Enter opens.",
                        self.servers.len(),
                        self.dms.len()
                    ));
                    // Without a package, the lists came from Discord, so it
                    // accepted the token.
                    if self.package.is_none() {
                        self.remember_token();
                    }
                }
                Err(error) => {
                    self.error = Some(format!(
                        "{error} Press s to change the token or package, or r to retry."
                    ))
                }
            },
            Loaded::Channels {
                guild_id, result, ..
            } => {
                let Some(server) = self.servers.iter_mut().find(|s| s.id == guild_id) else {
                    return;
                };
                server.loading = false;
                match result {
                    Ok(channels) => server.channels = Some(channels),
                    Err(error) => {
                        server.expanded = false;
                        self.error = Some(error.to_string());
                    }
                }
            }
            Loaded::Messages {
                channel_id,
                older,
                result,
                ..
            } => {
                let Some(history) = self.history.as_mut().filter(|h| h.channel.id == channel_id)
                else {
                    return;
                };
                history.loading = false;
                match result {
                    Ok(messages) => {
                        history.more = messages.len() == discord_purge_core::api::HISTORY_PAGE;
                        if older {
                            history.messages.extend(messages);
                        } else {
                            history.messages = messages;
                            history.cursor = 0;
                        }
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Loaded::Preview { revision, result } => match result {
                Ok(_) if revision != self.revision => {
                    self.notice = Some(
                        "The selection changed during the preview. Press p to preview again."
                            .into(),
                    )
                }
                Ok(run) => {
                    if self.package.is_none() {
                        self.account = run.account_label.clone();
                    }
                    self.preview = Some((run, revision));
                    self.modal = Some(Modal::Preview { cursor: 0 });
                }
                Err(error) => self.error = Some(error.to_string()),
            },
            Loaded::Confirmed { result } => {
                if result.is_ok() {
                    self.remember_token();
                }
                if let Err(error) = result.and_then(|(run, client)| self.begin_run(run, client)) {
                    self.error = Some(format!(
                        "The run did not start: {error} Correct the problem and press d again."
                    ));
                }
            }
        }
    }

    /// Save the token that Discord accepted, so the CLI and web panel use it.
    /// A failed save does not stop the work; it only shows the reason.
    fn remember_token(&mut self) {
        let Some(token) = self.token.as_deref() else {
            return;
        };
        let note = match self.config.token_store.remember(token) {
            Ok(true) => "Token saved for the CLI and web panel.".to_string(),
            Ok(false) => return,
            Err(error) => error.to_string(),
        };
        self.notice = Some(match self.notice.take() {
            Some(notice) => format!("{notice} {note}"),
            None => note,
        });
    }

    pub async fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        while let Some(result) = self.loads.try_join_next() {
            match result {
                Ok(loaded) => self.apply(loaded),
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    self.error = Some(format!("A list load stopped: {error}. Press r to reload."))
                }
            }
        }
        if let Some(job) = self.job.take_if(|job| job.is_finished()) {
            match job.await {
                Ok(loaded) => self.apply(loaded),
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    self.error = Some(format!("The task stopped: {error}. Press p to retry."))
                }
            }
        }
        if self.screen == Screen::Running
            && let Some(stats) = self.stats.as_ref().filter(|stats| stats.is_finished())
        {
            let snapshot = stats.snapshot();
            self.report = resume::write_report(&self.account, &snapshot, None)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|error| {
                    format!("Report failed: {error}. Check directory permissions.")
                });
            self.screen = Screen::Done;
        }
    }

    pub fn paste(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let field = match &mut self.modal {
            Some(Modal::Source(form)) => match form.field {
                1 => &mut form.token,
                2 => &mut form.package,
                _ => return,
            },
            Some(Modal::Link { input, .. }) => input,
            Some(Modal::Filters(form)) => match form.field {
                0 => &mut form.content,
                2 => &mut form.after,
                3 => &mut form.before,
                5 => &mut form.workers,
                6 => &mut form.delay,
                7 => &mut form.jitter,
                _ => return,
            },
            None if self.typing_query => &mut self.query,
            _ => return,
        };
        field.push_str(&text);
    }

    pub fn key(&mut self, code: KeyCode) {
        match self.screen {
            Screen::Running => {
                if let Some(c) = &self.controls {
                    match code {
                        KeyCode::Char('p' | ' ') => c.toggle_pause(),
                        KeyCode::Char('s' | 'q') | KeyCode::Esc => c.stop(),
                        _ => {}
                    }
                }
                return;
            }
            Screen::Done => {
                match code {
                    KeyCode::Enter => {
                        // Deleted messages are gone, so start from fresh history
                        // and a range that names no deleted message.
                        self.screen = Screen::Workspace;
                        self.history = None;
                        self.range = Range::All;
                        self.preview = None;
                        self.focus = Focus::Where;
                        self.stats = None;
                        self.controls = None;
                        self.changed();
                    }
                    KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                    _ => {}
                }
                return;
            }
            Screen::Workspace => {}
        }
        self.error = None;
        self.notice = None;
        if self.modal.is_some() {
            self.modal_key(code);
            return;
        }
        if self.typing_query {
            match code {
                KeyCode::Esc => {
                    self.typing_query = false;
                    self.query.clear();
                }
                KeyCode::Enter | KeyCode::Down => self.typing_query = false,
                code => {
                    edit(&mut self.query, code);
                    self.cursor = 0;
                }
            }
            return;
        }
        match code {
            KeyCode::Esc if self.job.is_some() => {
                if let Some(job) = self.job.take() {
                    job.abort();
                }
                self.notice = Some("Cancelled. Nothing was deleted.".into());
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('s') => self.open_source(false),
            KeyCode::Char('f') => self.open_filters(),
            KeyCode::Char('l') => {
                self.modal = Some(Modal::Link {
                    input: String::new(),
                    error: None,
                })
            }
            KeyCode::Char('r') => self.load_catalog(),
            KeyCode::Char('p') => self.start_preview(),
            KeyCode::Char('d') => self.ask_delete(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Where if self.history.is_some() => Focus::Messages,
                    _ => Focus::Where,
                }
            }
            code => match self.focus {
                Focus::Where => self.where_key(code),
                Focus::Messages => self.messages_key(code),
            },
        }
    }

    fn where_key(&mut self, code: KeyCode) {
        let rows = self.rows();
        self.cursor = self.cursor.min(rows.len() - 1);
        if move_cursor(&mut self.cursor, rows.len() - 1, code) {
            return;
        }
        let row = rows[self.cursor];
        match (code, row) {
            (KeyCode::Char('/'), _) => self.typing_query = true,
            (KeyCode::Esc, _) if !self.query.is_empty() => self.query.clear(),
            (KeyCode::Char(' '), _) => {
                self.toggle(row, &rows);
                self.changed();
            }
            (KeyCode::Right | KeyCode::Enter, Row::Server(index)) => {
                if code == KeyCode::Enter && self.servers[index].expanded {
                    self.servers[index].expanded = false;
                } else {
                    self.servers[index].expanded = true;
                    self.load_channels(index);
                }
            }
            (KeyCode::Right | KeyCode::Enter, Row::Channel(index, channel_index)) => {
                let channel = self.servers[index]
                    .channels
                    .as_deref()
                    .and_then(|channels| channels.get(channel_index))
                    .cloned();
                if let Some(channel) = channel {
                    self.open_history(channel);
                }
            }
            (KeyCode::Right | KeyCode::Enter, Row::Dm(index)) => {
                let channel = self.dms[index].channel.clone();
                self.open_history(channel);
            }
            (KeyCode::Left, Row::Server(index)) => self.servers[index].expanded = false,
            (KeyCode::Left, Row::Channel(index, _)) => {
                self.cursor = rows
                    .iter()
                    .position(|r| *r == Row::Server(index))
                    .unwrap_or(0)
            }
            _ => {}
        }
    }

    fn toggle(&mut self, row: Row, rows: &[Row]) {
        match row {
            Row::ServersHeader => {
                let shown: Vec<usize> = rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::Server(index) => Some(*index),
                        _ => None,
                    })
                    .collect();
                let on = !shown.iter().all(|&index| self.servers[index].all);
                for index in shown {
                    self.servers[index].all = on;
                    self.servers[index].picked.clear();
                }
            }
            Row::Server(index) => {
                let server = &mut self.servers[index];
                server.all = !server.all;
                server.picked.clear();
            }
            Row::Channel(index, channel_index) => {
                let server = &mut self.servers[index];
                let channels = server.channels.as_deref().unwrap_or_default();
                let id = channels[channel_index].id.clone();
                if server.all {
                    server.all = false;
                    server.picked = channels.iter().map(|c| c.id.clone()).collect();
                    server.picked.remove(&id);
                } else if !server.picked.remove(&id) {
                    server.picked.insert(id);
                }
            }
            Row::DmsHeader => {
                let shown: Vec<usize> = rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::Dm(index) => Some(*index),
                        _ => None,
                    })
                    .collect();
                let on = !shown.iter().all(|&index| self.dms[index].checked);
                for index in shown {
                    self.dms[index].checked = on;
                }
            }
            Row::Dm(index) => self.dms[index].checked = !self.dms[index].checked,
        }
    }

    fn messages_key(&mut self, code: KeyCode) {
        let Some(history) = &mut self.history else {
            self.focus = Focus::Where;
            return;
        };
        let last = history.messages.len().saturating_sub(1);
        if code == KeyCode::Down && history.cursor == last && history.more {
            self.load_messages(true);
            return;
        }
        if move_cursor(&mut history.cursor, last, code) {
            return;
        }
        let channel = history.channel.clone();
        let message = history.messages.get(history.cursor).cloned();
        match (code, message) {
            (KeyCode::Left | KeyCode::Esc, _) => self.focus = Focus::Where,
            (KeyCode::Char('c'), _) => {
                self.range = Range::All;
                self.changed();
                self.notice = Some("Range cleared: all your messages in the scope.".into());
            }
            (KeyCode::Char(' '), Some(m)) => {
                if !self.own(&m) {
                    self.error = Some("You can mark only your own messages. To mark pinned messages, turn on pinned messages in Filters (f).".into());
                    return;
                }
                let reference = message_ref(&m);
                let range = match std::mem::replace(&mut self.range, Range::All) {
                    Range::Only(mut set) => {
                        if !set.remove(&reference) {
                            set.insert(reference);
                        }
                        Range::Only(set)
                    }
                    _ => Range::Only(BTreeSet::from([reference])),
                };
                self.set_range(range, &channel);
                if let Some(h) = &mut self.history {
                    h.cursor = (h.cursor + 1).min(h.messages.len().saturating_sub(1));
                }
            }
            (KeyCode::Char('o'), Some(m)) => {
                self.set_range(Range::Older(message_ref(&m)), &channel)
            }
            (KeyCode::Char('n'), Some(m)) => {
                self.set_range(Range::Newer(message_ref(&m)), &channel)
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, code: KeyCode) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        self.modal = match modal {
            Modal::Help => None,
            Modal::Confirm => {
                if code == KeyCode::Char('y') {
                    self.start_run();
                } else {
                    self.notice = Some("Deletion cancelled. Nothing was deleted.".into());
                }
                None
            }
            Modal::Preview { mut cursor } => {
                let last = self
                    .preview_ready()
                    .map_or(0, |p| p.jobs.len().saturating_sub(1));
                match code {
                    KeyCode::Char('d') => {
                        self.ask_delete();
                        return;
                    }
                    KeyCode::Char('p') => {
                        self.start_preview();
                        None
                    }
                    code if move_cursor(&mut cursor, last, code) => Some(Modal::Preview { cursor }),
                    _ => None,
                }
            }
            Modal::LinkRange(reference) => {
                let channel_id = reference.channel_id.clone();
                let range = match code {
                    KeyCode::Char('o') => Some(Range::Older(reference)),
                    KeyCode::Char('n') => Some(Range::Newer(reference)),
                    KeyCode::Char(' ') => {
                        Some(match std::mem::replace(&mut self.range, Range::All) {
                            Range::Only(mut set) => {
                                set.insert(reference);
                                Range::Only(set)
                            }
                            _ => Range::Only(BTreeSet::from([reference])),
                        })
                    }
                    _ => None,
                };
                if let Some(range) = range {
                    // The link already added its channel to the scope.
                    self.range = range;
                    self.changed();
                    self.notice = Some(format!(
                        "Range set from the linked message in channel {channel_id}."
                    ));
                }
                None
            }
            Modal::Link { mut input, error } => match code {
                KeyCode::Esc => None,
                KeyCode::Enter => match parse_link(&input) {
                    Ok(link) => self.apply_link(link),
                    Err(e) => Some(Modal::Link {
                        input,
                        error: Some(e.to_string()),
                    }),
                },
                code => {
                    edit(&mut input, code);
                    Some(Modal::Link { input, error })
                }
            },
            Modal::Source(form) => self.source_key(form, code),
            Modal::Filters(form) => self.filters_key(form, code),
        };
    }

    fn apply_link(&mut self, link: Link) -> Option<Modal> {
        let channel = self
            .servers
            .iter()
            .flat_map(|server| server.channels.iter().flatten())
            .chain(self.dms.iter().map(|dm| &dm.channel))
            .find(|c| c.id == link.channel_id)
            .cloned()
            .unwrap_or_else(|| Channel {
                id: link.channel_id.clone(),
                name: Some(match &link.guild_id {
                    Some(_) => format!("channel {}", link.channel_id),
                    None => format!("DM {}", link.channel_id),
                }),
                kind: if link.guild_id.is_some() { 0 } else { 1 },
                guild_id: link.guild_id.clone(),
                recipients: Vec::new(),
            });
        self.ensure_in_scope(&channel);
        self.changed();
        if self.notice.is_none() {
            self.notice = Some(format!("{} is already in the scope.", channel.label()));
        }
        link.message_id.map(|message_id| {
            Modal::LinkRange(MessageRef {
                channel_id: link.channel_id,
                message_id,
            })
        })
    }

    fn source_key(&mut self, mut form: SourceForm, code: KeyCode) -> Option<Modal> {
        match code {
            KeyCode::Esc => return None,
            KeyCode::Up | KeyCode::BackTab => form.field = form.field.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => form.field = (form.field + 1).min(2),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 0 => {
                form.package_mode = !form.package_mode
            }
            KeyCode::Enter => {
                let token = form.token.trim().to_string();
                if !token.is_empty() {
                    self.token = Some(token);
                }
                if form.for_delete {
                    if self.token.is_none() {
                        form.error = Some("You entered no token. Paste your account token to delete, or press Esc to keep the preview only.".into());
                        return Some(Modal::Source(form));
                    }
                    return Some(Modal::Confirm);
                }
                if form.package_mode {
                    let path = PathBuf::from(form.package.trim());
                    if form.package.trim().is_empty() || !path.exists() {
                        form.error = Some(format!(
                            "The package path {:?} does not exist. Enter the path of the Discord data package ZIP or its extracted folder.",
                            form.package.trim()
                        ));
                        return Some(Modal::Source(form));
                    }
                    self.package = Some(path);
                } else {
                    if self.token.is_none() {
                        form.error = Some("You entered no token. Paste your account token, or choose Data package to browse offline.".into());
                        return Some(Modal::Source(form));
                    }
                    self.package = None;
                }
                self.load_catalog();
                return None;
            }
            code => {
                let field = match form.field {
                    1 => &mut form.token,
                    2 => &mut form.package,
                    _ => return Some(Modal::Source(form)),
                };
                edit(field, code);
            }
        }
        Some(Modal::Source(form))
    }

    fn filters_key(&mut self, mut form: FilterForm, code: KeyCode) -> Option<Modal> {
        match code {
            KeyCode::Esc => return None,
            KeyCode::Up | KeyCode::BackTab => form.field = form.field.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => form.field = (form.field + 1).min(7),
            KeyCode::Left if form.field == 1 => {
                form.type_cursor = form.type_cursor.saturating_sub(1)
            }
            KeyCode::Right if form.field == 1 => {
                form.type_cursor = (form.type_cursor + 1).min(TYPES.len() - 1)
            }
            KeyCode::Char(' ') if form.field == 1 => {
                form.types[form.type_cursor] = !form.types[form.type_cursor]
            }
            KeyCode::Char(' ') if form.field == 4 => form.pinned = !form.pinned,
            KeyCode::Enter => match self.apply_filters(&form) {
                Ok(()) => return None,
                Err(error) => form.error = Some(error.to_string()),
            },
            code => {
                let field = match form.field {
                    0 => &mut form.content,
                    2 => &mut form.after,
                    3 => &mut form.before,
                    5 => &mut form.workers,
                    6 => &mut form.delay,
                    7 => &mut form.jitter,
                    _ => return Some(Modal::Filters(form)),
                };
                edit(field, code);
            }
        }
        Some(Modal::Filters(form))
    }

    fn apply_filters(&mut self, form: &FilterForm) -> anyhow::Result<()> {
        let types: Vec<&str> = TYPES
            .iter()
            .zip(form.types)
            .filter(|(_, on)| *on)
            .map(|(t, _)| *t)
            .collect();
        let filter = Filter::default().with_inputs(
            Some(&form.content),
            Some(&types.join(",")),
            Some(form.after.trim()),
            Some(form.before.trim()),
        )?;
        let number = |name: &str, value: &str, accepted: &str| {
            value
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow::anyhow!("You entered {value:?} for {name}. Use {accepted}."))
        };
        let workers = form.workers.trim().parse::<usize>().map_err(|_| {
            anyhow::anyhow!(
                "You entered {:?} for workers. Use a whole number from 1 to 16.",
                form.workers
            )
        })?;
        let settings = EngineConfig::new(
            workers,
            number("delay", &form.delay, "seconds from 0 to 3600, such as 1.1")?,
            number(
                "jitter",
                &form.jitter,
                "a fraction from 0 to 1, such as 0.4",
            )?,
        )?;
        self.config.filter = filter;
        self.config.settings = settings;
        self.config.include_pinned = form.pinned;
        self.changed();
        self.notice = Some("Filters and speed saved. Press p to preview.".into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> App {
        App::new(Options {
            token: None,
            package: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/package.zip"),
            ),
            guild: None,
            channel: None,
            settings: Default::default(),
            filter: Filter::default(),
            include_pinned: false,
            token_store: discord_purge_core::resume::TokenStore::Off,
        })
    }

    async fn settle(app: &mut App) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.busy() {
                tokio::time::sleep(Duration::from_millis(1)).await;
                app.tick().await;
            }
        })
        .await
        .unwrap();
    }

    fn cursor_to(app: &mut App, row: Row) {
        app.cursor = app.rows().iter().position(|r| *r == row).unwrap();
    }

    #[tokio::test]
    async fn package_workspace_previews_exact_selection_and_needs_confirmation() {
        let mut app = fixture();
        app.start();
        settle(&mut app).await;
        assert!(
            app.listed && app.modal.is_none(),
            "lists load with no key press"
        );
        assert_eq!(app.servers[0].id, "900");
        assert_eq!(app.dms[0].channel.id, "222");

        // Everything: both section headers, then preview.
        cursor_to(&mut app, Row::ServersHeader);
        app.key(KeyCode::Char(' '));
        cursor_to(&mut app, Row::DmsHeader);
        app.key(KeyCode::Char(' '));
        app.key(KeyCode::Char('p'));
        settle(&mut app).await;
        assert_eq!(app.preview_ready().unwrap().total, 50);
        assert!(matches!(app.modal, Some(Modal::Preview { .. })));
        app.key(KeyCode::Esc);
        assert!(app.modal.is_none() && app.stats.is_none());

        // Marked messages across a server channel and a DM.
        cursor_to(&mut app, Row::Server(0));
        app.key(KeyCode::Right);
        settle(&mut app).await;
        cursor_to(&mut app, Row::Channel(0, 0));
        app.key(KeyCode::Enter);
        settle(&mut app).await;
        assert_eq!(app.focus, Focus::Messages);
        app.key(KeyCode::Char(' '));
        app.key(KeyCode::Char(' '));
        app.key(KeyCode::Left);
        cursor_to(&mut app, Row::Dm(0));
        app.key(KeyCode::Enter);
        settle(&mut app).await;
        app.key(KeyCode::Char(' '));
        assert!(
            app.preview_ready().is_none(),
            "a change makes the preview out of date"
        );
        app.key(KeyCode::Char('d'));
        settle(&mut app).await;
        let run = app.preview_ready().unwrap();
        assert_eq!(run.total, 3);
        assert_eq!(run.jobs.len(), 2);

        // Delete needs a token, then y. Any other key cancels.
        app.key(KeyCode::Char('d'));
        assert!(matches!(
            app.modal,
            Some(Modal::Source(SourceForm {
                for_delete: true,
                ..
            }))
        ));
        app.paste("not-a-real-token");
        app.key(KeyCode::Enter);
        assert!(matches!(app.modal, Some(Modal::Confirm)));
        app.key(KeyCode::Char('n'));
        assert!(app.modal.is_none() && app.stats.is_none() && app.job.is_none());

        // Older than a boundary, in the DM history that is still open.
        assert_eq!(app.focus, Focus::Messages);
        app.key(KeyCode::Char('c'));
        app.key(KeyCode::Down);
        let boundary = app.history.as_ref().unwrap().messages[1]
            .id
            .parse::<u64>()
            .unwrap();
        app.key(KeyCode::Char('o'));
        app.key(KeyCode::Char('p'));
        settle(&mut app).await;
        let run = app.preview_ready().unwrap();
        assert!(run.total > 0);
        assert!(
            run.jobs
                .iter()
                .flat_map(|channel_index| &channel_index.items)
                .all(|&message| message < boundary)
        );
        assert!(app.stats.is_none());
    }

    #[tokio::test]
    async fn filters_form_validates_and_name_filter_narrows_rows() {
        let mut app = fixture();
        app.start();
        settle(&mut app).await;
        app.key(KeyCode::Char('f'));
        app.key(KeyCode::Tab);
        app.key(KeyCode::Tab);
        app.paste("2026-13-01");
        app.key(KeyCode::Enter);
        let Some(Modal::Filters(form)) = &app.modal else {
            panic!("an invalid date keeps the form open");
        };
        assert!(form.error.as_deref().unwrap().contains("YYYY-MM-DD"));
        app.key(KeyCode::Esc);
        assert!(app.config.filter.after.is_none());

        app.key(KeyCode::Char('/'));
        app.paste("no such name");
        assert_eq!(app.rows(), vec![Row::ServersHeader, Row::DmsHeader]);
        app.key(KeyCode::Esc);
        assert_eq!(app.rows().len(), 4);
    }

    #[tokio::test]
    async fn pasted_dm_link_adds_scope_and_asks_for_the_range() {
        let mut app = fixture();
        app.start();
        settle(&mut app).await;
        app.key(KeyCode::Char('l'));
        app.paste("https://discord.com/channels/@me/333/444");
        app.key(KeyCode::Enter);
        assert!(
            app.dms
                .iter()
                .any(|dm| dm.channel.id == "333" && dm.checked)
        );
        assert!(matches!(app.modal, Some(Modal::LinkRange(_))));
        app.key(KeyCode::Char('n'));
        assert!(matches!(&app.range, Range::Newer(r) if r.message_id == "444"));
    }

    #[test]
    fn links_parse_and_bad_input_names_the_fix() {
        assert_eq!(
            parse_link("https://ptb.discord.com/channels/900/111/12345").unwrap(),
            Link {
                guild_id: Some("900".into()),
                channel_id: "111".into(),
                message_id: Some("12345".into())
            }
        );
        assert_eq!(parse_link(" 222 ").unwrap().channel_id, "222");
        let error = parse_link("https://example.com/channels/@me/1").unwrap_err();
        assert!(error.to_string().contains("Copy Link"));
        assert!(parse_link("https://discord.com/channels/@me/01").is_err());
    }
}
