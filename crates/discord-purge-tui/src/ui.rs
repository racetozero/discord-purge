//! Drawing for the terminal workspace, the run, and the forms.
//!
//! The layout has few boxes: a header line, the Where and Messages panes (side
//! by side when wide, one at a time when narrow), the Plan as one sentence with
//! a count, a status line, and a footer that the key table writes. Colors are
//! ANSI, so they follow the terminal theme; `NO_COLOR` turns them off.
use std::sync::OnceLock;

use discord_purge_core::RunHandle;
use discord_purge_core::engine::{RunPhase, Snapshot};
use discord_purge_core::filter::{TYPE_NAMES, snowflake_time as snowflake_created};
use discord_purge_core::resume::format_duration;
use ratatui::prelude::*;
use ratatui::symbols;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, LineGauge, List, ListItem, ListState,
    Paragraph, Sparkline, Wrap,
};

use crate::app::{
    App, FilterField, FilterForm, Focus, HistoryLoad, Modal, Range, Row, Screen, SourceField,
    SourceForm, message_ref,
};
use crate::input::TextInput;
use crate::keys::{self, BINDINGS, Ctx};

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(crate) const MIN_WIDTH: u16 = 40;
pub(crate) const MIN_HEIGHT: u16 = 12;
/// From this width, Where and Messages show side by side.
const WIDE: u16 = 96;

fn no_color() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()))
}

fn fg(color: Color) -> Style {
    if no_color() {
        Style::default()
    } else {
        Style::default().fg(color)
    }
}

/// Yellow marks what is selected or needs attention; red is only for deletion.
fn accent() -> Style {
    fg(Color::Yellow)
}

fn danger() -> Style {
    fg(Color::Red)
}

fn ok() -> Style {
    fg(Color::Green)
}

fn info() -> Style {
    fg(Color::Cyan)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Display width, so CJK and emoji names line up.
fn width(text: &str) -> usize {
    Span::raw(text).width()
}

fn pad(text: &str, to: usize) -> String {
    format!("{text}{}", " ".repeat(to.saturating_sub(width(text))))
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn snowflake_time(id: &str) -> String {
    id.parse::<u64>()
        .ok()
        .and_then(snowflake_created)
        .map(|created| created.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn spinner(app: &App) -> &'static str {
    SPINNER[app.frame % SPINNER.len()]
}

/// Keys and labels in one line, cut from the end to `max` columns. The first
/// pair always stays, so put the most needed key first.
fn hint_line(pairs: &[(&str, &str)], max: usize) -> Line<'static> {
    let mut pairs = pairs.to_vec();
    let size = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, a)| width(k) + width(a) + 3)
            .sum::<usize>()
    };
    while pairs.len() > 1 && size(&pairs) > max {
        pairs.pop();
    }
    let mut spans = vec![Span::raw(" ")];
    for (key, action) in pairs {
        spans.push(Span::styled(
            key.to_string(),
            accent().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {action}  "), dim()));
    }
    Line::from(spans)
}

/// A title row for a pane or a sheet: bold and accented when focused.
fn title_row(title: &str, focused: bool, detail: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(if focused { "▍" } else { " " }, accent()),
        Span::styled(
            title.to_string(),
            if focused {
                accent().add_modifier(Modifier::BOLD)
            } else {
                bold()
            },
        ),
        Span::raw("  "),
    ];
    spans.extend(detail);
    Line::from(spans)
}

/// A bordered card over the screen. Returns the area inside its padding.
fn card(frame: &mut Frame, rect: Rect, title: &str, border: Style) -> Rect {
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(
            format!(" {title} "),
            border.add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner.inner(Margin::new(1, 0))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// The rows that `lines` need at `inner` columns when they wrap.
fn wrapped_height(lines: &[Line], inner: u16) -> u16 {
    let inner = inner.max(1) as usize;
    lines
        .iter()
        .map(|l| l.width().div_ceil(inner).max(1) as u16)
        .sum()
}

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Enlarge the terminal to at least {MIN_WIDTH} columns and {MIN_HEIGHT} rows. Press q or Esc to stop or quit."
            ))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    match &app.screen {
        Screen::Workspace => draw_workspace(frame, area, app),
        Screen::Running(RunHandle { controls, .. }) | Screen::Done { controls, .. } => {
            let [header, body, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .areas(area);
            draw_header(frame, header, app);
            let snapshot = controls.stats().snapshot();
            let ctx = match &app.screen {
                Screen::Running(_) => {
                    draw_run(frame, body, app, &snapshot);
                    Ctx::Run
                }
                _ => {
                    draw_done(frame, body, app, &snapshot);
                    Ctx::Done
                }
            };
            let pairs: Vec<(&str, &str)> = keys::footer(&[ctx])
                .iter()
                .map(|b| (b.key_label, b.short))
                .collect();
            frame.render_widget(hint_line(&pairs, footer.width as usize), footer);
        }
    }
}

fn draw_workspace(frame: &mut Frame, area: Rect, app: &App) {
    let status = status_line(app);
    let status_height = wrapped_height(&status, area.width).clamp(1, 3);
    let [header, _, body, rule, plan, status_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(area.height >= 20)),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(plan_height(area.width)),
        Constraint::Length(status_height),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(frame, header, app);
    frame.render_widget(
        Paragraph::new(status).wrap(Wrap { trim: false }),
        status_area,
    );
    frame.render_widget(hint_line(&footer_pairs(app), footer.width as usize), footer);
    // With no source yet, the welcome card takes the plan's place too.
    let welcome =
        !app.listed && matches!(&app.modal, Some(Modal::Source(form)) if !form.for_delete);
    if welcome {
        draw_welcome(frame, body.union(plan), app);
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled("─".repeat(area.width as usize), dim())),
        rule,
    );
    draw_plan(frame, plan, app);

    let wide = area.width >= WIDE;
    let sheet = matches!(
        app.modal,
        Some(
            Modal::Source(_)
                | Modal::Filters(_)
                | Modal::Preview { .. }
                | Modal::Link { .. }
                | Modal::LinkRange(_)
        )
    );
    if wide {
        let [left, gap, right] = Layout::horizontal([
            Constraint::Percentage(36),
            Constraint::Length(3),
            Constraint::Min(20),
        ])
        .areas(body);
        draw_where(frame, left, app, false);
        frame.render_widget(
            Paragraph::new(vec![Line::styled(" │", dim()); gap.height as usize]),
            gap,
        );
        if sheet {
            draw_sheet(frame, right, app);
        } else {
            draw_messages(frame, right, app, false);
        }
    } else if sheet {
        draw_sheet(frame, body, app);
    } else {
        match app.focus {
            Focus::Where => draw_where(frame, body, app, true),
            Focus::Messages => draw_messages(frame, body, app, true),
        }
    }
    match &app.modal {
        Some(Modal::Confirm) => draw_confirm(frame, area, app),
        Some(Modal::Help) => draw_help(frame, area),
        Some(Modal::Palette { input, cursor }) => draw_palette(frame, area, app, input, *cursor),
        _ => {}
    }
}

fn footer_pairs(app: &App) -> Vec<(&'static str, &'static str)> {
    match &app.modal {
        Some(Modal::Source(form)) if form.for_delete => {
            vec![("Esc", "close"), ("Enter", "continue")]
        }
        Some(Modal::Source(form)) => {
            let mut pairs = vec![("Esc", "close"), ("↑↓", "field")];
            if form.field == SourceField::Kind {
                pairs.push(("←→", "source"));
            }
            pairs.push(("Enter", "continue"));
            pairs
        }
        Some(Modal::Filters(_)) => vec![
            ("Esc", "cancel"),
            ("↑↓", "field"),
            ("←→ Space", "types"),
            ("Enter", "save"),
        ],
        Some(Modal::Preview { .. }) => vec![
            ("Esc", "close"),
            ("↑↓", "move"),
            ("d", "delete these"),
            ("p", "count again"),
        ],
        Some(Modal::Link { .. }) => vec![("Esc", "close"), ("Enter", "add to scope")],
        Some(Modal::LinkRange(_)) => vec![
            ("Esc", "only add the channel"),
            ("o", "older"),
            ("n", "newer"),
            ("Space", "mark it"),
        ],
        Some(Modal::Confirm) => vec![("any key", "cancel"), ("y", "delete")],
        Some(Modal::Help) => vec![("any key", "close")],
        Some(Modal::Palette { .. }) => {
            vec![("Esc", "close"), ("↑↓", "choose"), ("Enter", "run")]
        }
        None if app.typing_query => vec![
            ("Esc", "clear"),
            ("type", "find by name"),
            ("Enter", "keep"),
        ],
        None => {
            let mut bindings = keys::footer(&app.contexts());
            // The key overlay goes first, so the cut keeps it.
            bindings.sort_by_key(|b| b.action != keys::Action::Help);
            bindings.iter().map(|b| (b.key_label, b.short)).collect()
        }
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let brand = if no_color() {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled(" discord-purge ", brand), Span::raw("  ")];
    match &app.package {
        Some(path) => {
            spans.push(Span::styled(
                clean(&path.file_name().unwrap_or_default().to_string_lossy()),
                bold(),
            ));
            spans.push(Span::styled(
                if app.token.is_some() {
                    "  data package · token ready".to_string()
                } else {
                    "  data package · preview only until you add a token".to_string()
                },
                dim(),
            ));
        }
        None if app.account.is_empty() => {
            spans.push(Span::styled("Not signed in · press s", dim()))
        }
        None => {
            spans.push(Span::styled(clean(&app.account), bold()));
            spans.push(Span::styled("  Discord account", dim()));
        }
    }
    // The activity takes the right end; the source text gets the rest.
    let activity = if app.busy() && !app.activity.is_empty() {
        format!(" {} {}… ", spinner(app), clean(&app.activity))
    } else {
        String::new()
    };
    let [source, busy] = Layout::horizontal([
        Constraint::Min(16),
        Constraint::Length(width(&activity) as u16),
    ])
    .areas(area);
    frame.render_widget(Line::from(spans), source);
    frame.render_widget(Line::styled(activity, accent()).right_aligned(), busy);
}

fn check(on: bool, partial: bool) -> (&'static str, Style) {
    match (on, partial) {
        (true, _) => ("●", accent()),
        (false, true) => ("◐", accent()),
        _ => ("○", dim()),
    }
}

/// A list of one-row items with a cursor bar at `selected`. The cursor row
/// is only bold, so its marks, such as the red deletion mark, keep their
/// color; the bar alone shows focus.
fn render_list(
    frame: &mut Frame,
    area: Rect,
    items: Vec<ListItem>,
    selected: usize,
    focused: bool,
) {
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(if focused { bold() } else { Style::default() });
    frame.render_stateful_widget(list, area, &mut state);
    let row = area.y + selected.saturating_sub(state.offset()) as u16;
    if row < area.bottom() {
        frame.buffer_mut()[(area.x, row)].set_style(if focused { accent() } else { dim() });
    }
}

/// Where and Messages as tabs, for a narrow terminal that shows one pane.
fn breadcrumb(app: &App) -> Vec<Span<'static>> {
    let other = match app.focus {
        Focus::Where if app.history.is_some() => "Messages",
        Focus::Where => return Vec::new(),
        Focus::Messages => "Where",
    };
    vec![Span::styled(format!("Tab {other}"), dim())]
}

fn draw_where(frame: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let focused = app.focus == Focus::Where && app.modal.is_none();
    let chosen = app
        .servers
        .iter()
        .filter(|s| s.all || !s.picked.is_empty())
        .count()
        + app.dms.iter().filter(|dm| dm.checked).count();
    let mut detail = vec![Span::styled(
        if app.listed {
            format!("{chosen} selected  ")
        } else {
            String::new()
        },
        dim(),
    )];
    if narrow {
        detail.extend(breadcrumb(app));
    }
    let [title, query, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(app.typing_query || !app.query.is_empty())),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(title_row("Where", focused, detail), title);
    if query.height > 0 {
        let mut spans = vec![Span::styled("  / ", accent())];
        spans.extend(input_spans(&app.query, app.typing_query, false, ""));
        frame.render_widget(Line::from(spans), query);
    }
    if !app.listed {
        let text = if app.busy() {
            format!(
                "  {} Loading your servers and direct messages…",
                spinner(app)
            )
        } else {
            "  No lists are loaded. Press s to sign in with a token or to open a data package."
                .to_string()
        };
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).style(dim()),
            list,
        );
        return;
    }
    let rows = app.rows();
    let inner = list.width.saturating_sub(2) as usize;
    let header = |name: &str, chosen: usize, total: usize| {
        let count = format!("{chosen}/{total}");
        ListItem::new(Line::from(vec![
            Span::styled(name.to_string(), dim().add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(inner.saturating_sub(width(name) + count.len() + 1).max(1))),
            Span::styled(count, dim()),
        ]))
    };
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            Row::ServersHeader => header(
                "SERVERS",
                app.servers
                    .iter()
                    .filter(|s| s.all || !s.picked.is_empty())
                    .count(),
                app.servers.len(),
            ),
            Row::Server(index) => {
                let s = &app.servers[index];
                let (mark, style) = check(s.all, !s.picked.is_empty());
                let mut spans = vec![
                    Span::styled(if s.expanded { "▾ " } else { "▸ " }, dim()),
                    Span::styled(format!("{mark} "), style),
                    Span::raw(clean(&s.name)),
                ];
                if s.loading {
                    spans.push(Span::styled(format!(" {}", spinner(app)), accent()));
                } else if !s.all && !s.picked.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", plural(s.picked.len(), "channel", "channels")),
                        dim(),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
            Row::Channel(index, channel_index) => {
                let s = &app.servers[index];
                let Some(c) = s
                    .channels
                    .as_deref()
                    .and_then(|channels| channels.get(channel_index))
                else {
                    return ListItem::new("");
                };
                let (mark, style) = check(s.all || s.picked.contains(&c.id), false);
                let name = c.label();
                let name = name.strip_prefix('#').unwrap_or(&name);
                ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{mark} "), style),
                    Span::styled("# ", dim()),
                    Span::raw(clean(name)),
                ]))
            }
            Row::DmsHeader => header(
                "DIRECT MESSAGES",
                app.dms.iter().filter(|dm| dm.checked).count(),
                app.dms.len(),
            ),
            Row::Dm(index) => {
                let dm = &app.dms[index];
                let (mark, style) = check(dm.checked, false);
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{mark} "), style),
                    Span::raw(clean(&dm.channel.label())),
                ]))
            }
        })
        .collect();
    let selected = app.cursor.min(rows.len().saturating_sub(1));
    render_list(frame, list, items, selected, focused);
}

/// The gutter mark and the text style of one message row.
enum Fate {
    /// The range boundary: it stays.
    Anchor,
    Marked,
    Deleted,
    /// Your message, outside the range.
    Kept,
    /// Another user's message, which the app never deletes.
    Other,
}

fn draw_messages(frame: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let focused = app.focus == Focus::Messages && app.modal.is_none();
    let [title, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let Some(history) = &app.history else {
        frame.render_widget(title_row("Messages", focused, Vec::new()), title);
        let key = |k: &str| Span::styled(pad(k, 8), accent().add_modifier(Modifier::BOLD));
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::styled("Open a channel or DM with → or Enter to read it here.", dim()),
                Line::from(""),
                Line::from(vec![key("Space"), Span::styled("mark single messages to delete", dim())]),
                Line::from(vec![key("o / n"), Span::styled("delete everything older / newer than a message", dim())]),
                Line::from(""),
                Line::styled(
                    "With no marks and no boundary, the plan deletes all your messages in the scope.",
                    dim(),
                ),
            ])
            .wrap(Wrap { trim: false }),
            list.inner(Margin::new(2, 0)),
        );
        return;
    };
    let channel = &history.channel;
    let server = channel
        .guild_id
        .as_ref()
        .and_then(|id| app.servers.iter().find(|s| &s.id == id))
        .map(|s| format!(" · {}", clean(&s.name)))
        .unwrap_or_default();
    let history_in_scope = app.history_in_scope();
    let mut detail = vec![
        Span::styled(clean(&channel.label()), bold()),
        Span::styled(format!("{server}  "), dim()),
        if history_in_scope {
            Span::styled("in scope  ", accent())
        } else {
            Span::styled("not in scope  ", dim())
        },
    ];
    if narrow {
        detail.extend(breadcrumb(app));
    }
    frame.render_widget(title_row("Messages", focused, detail), title);
    if history.messages.is_empty() {
        let text = match &history.load {
            HistoryLoad::Loading => format!("  {} Loading messages…", spinner(app)),
            HistoryLoad::Failed(reason) => format!(
                "  Cannot load these messages: {} Press Enter to retry.",
                clean(reason)
            ),
            HistoryLoad::Idle => "  This channel has no messages to show.".to_string(),
        };
        let style = if matches!(history.load, HistoryLoad::Failed(_)) {
            danger()
        } else {
            dim()
        };
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).style(style),
            list,
        );
        return;
    }
    let anchor = match &app.range {
        Range::Older(a) | Range::Newer(a) => Some(a),
        _ => None,
    };
    let name = |m: &discord_purge_core::api::BrowseMessage| {
        clean(
            m.author
                .global_name
                .as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or(&m.author.username),
        )
    };
    let author_width = history
        .messages
        .iter()
        .map(|m| width(&name(m)))
        .max()
        .unwrap_or(0)
        .clamp(3, 14);
    let rule_width = list.width.saturating_sub(4) as usize;
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected = 0;
    let mut last_day = None;
    let mut last_author: Option<&str> = None;
    for (index, message) in history.messages.iter().enumerate() {
        let created = message.id.parse::<u64>().ok().and_then(snowflake_created);
        let day = created.map(|c| c.date_naive());
        if day != last_day {
            let label = created
                .map(|c| c.format(" %a %-d %b %Y ").to_string())
                .unwrap_or_else(|| " unknown date ".into());
            items.push(ListItem::new(Line::from(vec![
                Span::styled("──", dim()),
                Span::styled(label.clone(), dim().add_modifier(Modifier::BOLD)),
                Span::styled(
                    "─".repeat(rule_width.saturating_sub(width(&label) + 2)),
                    dim(),
                ),
            ])));
            last_day = day;
            last_author = None;
        }
        if index == history.cursor {
            selected = items.len();
        }
        let reference = message_ref(message);
        let fate = if anchor == Some(&reference) {
            Fate::Anchor
        } else if matches!(&app.range, Range::Only(set) if set.contains(&reference))
            && app.in_range(message, history_in_scope)
        {
            Fate::Marked
        } else if app.in_range(message, history_in_scope) {
            Fate::Deleted
        } else if app.own(message) {
            Fate::Kept
        } else {
            Fate::Other
        };
        let (gutter, gutter_style) = match fate {
            Fate::Anchor => ("◆", accent().add_modifier(Modifier::BOLD)),
            Fate::Marked => ("●", danger().add_modifier(Modifier::BOLD)),
            Fate::Deleted => ("┃", danger()),
            Fate::Kept => ("·", dim()),
            Fate::Other => (" ", Style::default()),
        };
        let body = if matches!(fate, Fate::Other) {
            dim()
        } else {
            Style::default()
        };
        let author = name(message);
        let shown_author = if last_author == Some(message.author.id.as_str()) {
            String::new()
        } else {
            author
        };
        last_author = Some(message.author.id.as_str());
        let mut spans = vec![
            Span::styled(format!("{gutter} "), gutter_style),
            Span::styled(
                created
                    .map(|c| c.format("%H:%M ").to_string())
                    .unwrap_or_default(),
                dim(),
            ),
            Span::styled(
                format!("{} ", pad(&shown_author, author_width)),
                body.add_modifier(Modifier::BOLD),
            ),
        ];
        let text = clean(&message.content);
        if text.trim().is_empty() {
            spans.push(Span::styled(
                match message.attachments.first() {
                    Some(a) => format!("[{}]", clean(&a.filename)),
                    None => "[no text]".into(),
                },
                dim(),
            ));
        } else {
            spans.push(Span::styled(text, body));
            if !message.attachments.is_empty() {
                spans.push(Span::styled(
                    format!("  +{}", plural(message.attachments.len(), "file", "files")),
                    dim(),
                ));
            }
        }
        if message.pinned {
            spans.push(Span::styled("  pinned", accent()));
        }
        if matches!(fate, Fate::Anchor) {
            spans.push(Span::styled("  boundary, kept", accent()));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    items.push(ListItem::new(Span::styled(
        if history.load == HistoryLoad::Loading {
            format!("  {} Loading older messages…", spinner(app))
        } else if history.more {
            "  ↓ Move down to load older messages".into()
        } else {
            "  Start of the history".into()
        },
        dim(),
    )));
    render_list(frame, list, items, selected, focused);
}

fn scope_text(app: &App) -> Option<String> {
    let whole = app.servers.iter().filter(|s| s.all).count();
    let channels: usize = app.servers.iter().map(|s| s.picked.len()).sum();
    let dms = app.dms.iter().filter(|dm| dm.checked).count();
    let mut parts = Vec::new();
    if whole > 0 {
        parts.push(plural(whole, "whole server", "whole servers"));
    }
    if channels > 0 {
        parts.push(plural(channels, "channel", "channels"));
    }
    if dms > 0 {
        parts.push(plural(dms, "DM", "DMs"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn range_text(app: &App) -> String {
    match &app.range {
        Range::All => "all my messages".into(),
        Range::Older(a) => format!(
            "my messages older than the one from {}",
            snowflake_time(&a.message_id)
        ),
        Range::Newer(a) => format!(
            "my messages newer than the one from {}",
            snowflake_time(&a.message_id)
        ),
        Range::Only(set) if set.is_empty() => "no marked messages".into(),
        Range::Only(set) => plural(set.len(), "marked message", "marked messages"),
    }
}

fn filter_parts(app: &App) -> Vec<String> {
    let inputs = app.config.filter.inputs();
    let mut parts = Vec::new();
    if !inputs.types.is_empty() {
        parts.push(inputs.types.replace(',', " + "));
    }
    if !inputs.content.is_empty() {
        parts.push(format!("text “{}”", clean(&inputs.content)));
    }
    match (inputs.after_date.is_empty(), inputs.before_date.is_empty()) {
        (false, false) => parts.push(format!("{} to {}", inputs.after_date, inputs.before_date)),
        (false, true) => parts.push(format!("from {}", inputs.after_date)),
        (true, false) => parts.push(format!("before {}", inputs.before_date)),
        (true, true) => {}
    }
    if app.config.include_pinned {
        parts.push("pinned too".into());
    }
    parts
}

/// The count next to the plan: what the preview knows now.
fn count_badge(app: &App) -> Span<'static> {
    let ready = app.preview_ready();
    if app.job_running() {
        let doing = if ready.is_some() {
            "starting"
        } else {
            "counting"
        };
        return Span::styled(format!(" {} {doing} ", spinner(app)), accent());
    }
    match ready {
        Some(run) => {
            let count = if app.package.is_some() {
                format!("up to {}", plural(run.total(), "message", "messages"))
            } else if run.complete {
                plural(run.total(), "message", "messages")
            } else {
                format!("at least {}", plural(run.total(), "message", "messages"))
            };
            let style = if no_color() {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            };
            Span::styled(
                format!(
                    " {count} · {} ",
                    plural(run.jobs.len(), "channel", "channels")
                ),
                style,
            )
        }
        None if app.preview.is_some() => Span::styled(" changed · p to count again ", accent()),
        None if scope_text(app).is_none() => Span::raw(""),
        None => Span::styled(" not counted · p ", dim()),
    }
}

/// The plan rows: two when wide, three when narrow, so the sentence can wrap.
fn plan_height(width: u16) -> u16 {
    if width >= 80 { 2 } else { 3 }
}

fn draw_plan(frame: &mut Frame, area: Rect, app: &App) {
    let badge = count_badge(app);
    let [label, rest] =
        Layout::horizontal([Constraint::Length(7), Constraint::Min(10)]).areas(area);
    frame.render_widget(
        Line::styled(" Plan", accent().add_modifier(Modifier::BOLD)),
        label,
    );
    let sentence = match scope_text(app) {
        None => Line::styled(
            "Select a server, channel, or DM with Space. Press p to count the exact messages.",
            dim(),
        ),
        Some(scope) => {
            let mut spans = vec![
                Span::styled("Delete ", bold()),
                Span::raw(range_text(app)),
                Span::styled(" in ", dim()),
                Span::raw(scope),
            ];
            for part in filter_parts(app) {
                spans.push(Span::styled(" · ", dim()));
                spans.push(Span::raw(part));
            }
            Line::from(spans)
        }
    };
    let [top, bottom] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(rest);
    if area.height < 3 {
        // Wide: the sentence and the count on the top row, the speed below.
        let [text, count] = Layout::horizontal([
            Constraint::Min(10),
            Constraint::Length(badge.width() as u16 + 1),
        ])
        .areas(top);
        frame.render_widget(Paragraph::new(sentence), text);
        frame.render_widget(Line::from(badge).right_aligned(), count);
        let s = &app.config.settings;
        frame.render_widget(
            Line::styled(
                format!(
                    "{} · {:.1}s ±{:.0}% · {} · nothing is deleted until you confirm",
                    plural(s.workers, "worker", "workers"),
                    s.delay.as_secs_f64(),
                    s.jitter * 100.0,
                    if app.config.include_pinned {
                        "pinned deleted"
                    } else {
                        "pinned kept"
                    },
                ),
                dim(),
            ),
            bottom,
        );
    } else {
        // Narrow: the sentence wraps over two rows, and the count goes below.
        frame.render_widget(Paragraph::new(sentence).wrap(Wrap { trim: true }), top);
        frame.render_widget(Line::from(badge), bottom);
    }
}

fn status_line(app: &App) -> Vec<Line<'static>> {
    // An open form owns the status line, so its error shows on any size.
    let form_error = match &app.modal {
        Some(Modal::Filters(form)) => Some(form.parse().err().map(|e| e.to_string())),
        Some(Modal::Source(form)) => Some(form.error.clone()),
        Some(Modal::Link { error, .. }) => Some(error.clone()),
        _ => None,
    };
    match form_error {
        Some(Some(error)) => {
            return vec![Line::from(vec![
                Span::styled(" ✗ ", danger().add_modifier(Modifier::BOLD)),
                Span::styled(clean(&error), danger()),
            ])];
        }
        Some(None) if matches!(app.modal, Some(Modal::Filters(_))) => {
            return vec![Line::styled(" ✓ Valid. Enter saves.", ok())];
        }
        _ => {}
    }
    if let Some(error) = &app.error {
        vec![Line::from(vec![
            Span::styled(" ✗ ", danger().add_modifier(Modifier::BOLD)),
            Span::styled(clean(error), danger()),
            Span::styled("  Esc", dim()),
        ])]
    } else if let Some(notice) = &app.notice {
        vec![Line::from(vec![
            Span::styled(" • ", info()),
            Span::styled(clean(notice), info()),
        ])]
    } else {
        vec![Line::from("")]
    }
}

/// A text field: the text, and the cursor as a reversed cell when active.
fn input_spans(
    input: &TextInput,
    active: bool,
    mask: bool,
    placeholder: &str,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = if mask {
        vec!['•'; input.text.chars().count()]
    } else {
        clean(&input.text).chars().collect()
    };
    if !active {
        return vec![if chars.is_empty() {
            Span::styled(placeholder.to_string(), dim())
        } else {
            Span::raw(chars.iter().collect::<String>())
        }];
    }
    let at = input.cursor.min(chars.len());
    let cursor = chars.get(at).map_or(" ".to_string(), |c| c.to_string());
    let mut spans = vec![
        Span::styled(chars[..at].iter().collect::<String>(), bold()),
        Span::styled(cursor, Style::default().add_modifier(Modifier::REVERSED)),
    ];
    if at < chars.len() {
        spans.push(Span::styled(
            chars[at + 1..].iter().collect::<String>(),
            bold(),
        ));
    } else if chars.is_empty() && !placeholder.is_empty() {
        spans.push(Span::styled(placeholder.to_string(), dim()));
    }
    spans
}

fn field_line(active: bool, label: &str, mut value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            if active { " › " } else { "   " },
            accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad(label, 14),
            if active {
                accent().add_modifier(Modifier::BOLD)
            } else {
                dim()
            },
        ),
    ];
    spans.append(&mut value);
    Line::from(spans)
}

fn source_lines(app: &App, form: &SourceForm) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !form.for_delete {
        let mode = |on: bool, label: &str| {
            Span::styled(
                format!("{} {label}   ", if on { "●" } else { "○" }),
                if on {
                    accent().add_modifier(Modifier::BOLD)
                } else {
                    dim()
                },
            )
        };
        lines.push(field_line(
            form.field == SourceField::Kind,
            "Source",
            vec![
                mode(!form.package_mode, "Discord account"),
                mode(form.package_mode, "Data package"),
            ],
        ));
    }
    let token_active = form.field == SourceField::Token;
    let placeholder = if app.token.is_some() {
        "saved token in use"
    } else {
        "paste your token"
    };
    lines.push(field_line(
        token_active,
        "Token",
        input_spans(&form.token, token_active, true, placeholder),
    ));
    if !form.for_delete {
        let active = form.field == SourceField::Package;
        lines.push(field_line(
            active,
            "Package path",
            input_spans(&form.package, active, false, "drop or paste package.zip"),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if form.for_delete {
            "   Deletion needs your account token. The preview stays as it is."
        } else {
            "   A data package works offline and needs no token."
        },
        dim(),
    ));
    lines.extend(
        discord_purge_core::TOKEN_HELP
            .lines()
            .map(|l| Line::styled(format!("   {l}"), dim())),
    );
    lines
}

fn filter_lines(form: &FilterForm) -> Vec<Line<'static>> {
    let text = |field: FilterField, label: &str, input: &TextInput, placeholder: &str| {
        let active = form.field == field;
        field_line(
            active,
            label,
            input_spans(input, active, false, placeholder),
        )
    };
    let chips: Vec<Span> = TYPE_NAMES
        .iter()
        .enumerate()
        .map(|(index, t)| {
            let on = form.types[index];
            let mut style = if on {
                accent().add_modifier(Modifier::BOLD)
            } else {
                dim()
            };
            if form.field == FilterField::Types && form.type_cursor == index {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Span::styled(format!("{}{t}", if on { "●" } else { "○" }), style)
        })
        .flat_map(|chip| [chip, Span::raw(" ")])
        .collect();
    let pinned_active = form.field == FilterField::Pinned;
    let help = match form.field {
        FilterField::Content => "Words to find, or /regex/ or /regex/i. Case is ignored for words.",
        FilterField::Types => "←→ moves, Space turns a type on or off. No type means every type.",
        FilterField::After | FilterField::Before => {
            "A date as YYYY-MM-DD, in UTC. Leave empty for no limit."
        }
        FilterField::Pinned => "Space turns it on or off. Pinned messages are kept by default.",
        FilterField::Workers => "Channels deleted at the same time: a whole number from 1 to 16.",
        FilterField::Delay => "Base seconds between deletes in a channel: 0 to 3600.",
        FilterField::Jitter => "Random change to the delay, as a fraction from 0 to 1.",
    };
    vec![
        text(FilterField::Content, "Text", &form.content, "any text"),
        field_line(form.field == FilterField::Types, "Types", chips),
        text(FilterField::After, "From date", &form.after, "no limit"),
        text(FilterField::Before, "To date", &form.before, "no limit"),
        field_line(
            pinned_active,
            "Pinned",
            vec![Span::styled(
                if form.pinned {
                    "● delete pinned messages too"
                } else {
                    "○ keep pinned messages"
                },
                if form.pinned {
                    accent()
                } else {
                    Style::default()
                },
            )],
        ),
        Line::styled("   SPEED", dim().add_modifier(Modifier::BOLD)),
        text(FilterField::Workers, "Workers", &form.workers, ""),
        text(FilterField::Delay, "Delay (s)", &form.delay, ""),
        text(FilterField::Jitter, "Jitter", &form.jitter, ""),
        Line::from(""),
        Line::styled(format!("   {help}"), dim()),
    ]
}

fn preview_lines(app: &App, cursor: usize, rows: usize) -> Vec<Line<'static>> {
    let Some(run) = app.preview_ready() else {
        return vec![Line::styled(
            "   The preview is out of date. Press p to count again.",
            accent(),
        )];
    };
    let mut lines = vec![Line::from(vec![
        Span::raw("   "),
        Span::styled(
            plural(run.total(), "message", "messages"),
            accent().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " in {}. Nothing has been deleted.",
            plural(run.jobs.len(), "channel", "channels")
        )),
    ])];
    if app.package.is_some() {
        lines.push(Line::styled(
            "   A package cannot show pins or access, so this is a maximum.",
            dim(),
        ));
    }
    if !run.complete {
        lines.push(Line::styled(
            format!("   {}", discord_purge_core::search::INCOMPLETE_NOTICE),
            accent(),
        ));
    }
    lines.push(Line::from(""));
    let most = run
        .jobs
        .iter()
        .map(|j| j.items.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let shown = rows.saturating_sub(lines.len()).max(3);
    let start = cursor.saturating_sub(shown - 1);
    for (index, job) in run.jobs.iter().enumerate().skip(start).take(shown) {
        let bar = (job.items.len() * 12).div_ceil(most);
        let here = index == cursor;
        lines.push(Line::from(vec![
            Span::styled(if here { " ▌ " } else { "   " }, accent()),
            Span::styled(format!("{:>7}  ", job.items.len()), bold()),
            Span::styled("━".repeat(bar), danger()),
            Span::styled("─".repeat(12 - bar.min(12)), dim()),
            Span::raw("  "),
            Span::styled(
                clean(&job.label),
                if here { bold() } else { Style::default() },
            ),
        ]));
    }
    lines
}

fn draw_sheet(frame: &mut Frame, area: Rect, app: &App) {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let (name, lines) = match &app.modal {
        Some(Modal::Source(form)) => ("Source", source_lines(app, form)),
        Some(Modal::Filters(form)) => ("Filters and speed", filter_lines(form)),
        Some(Modal::Preview { cursor }) => (
            "Exact preview",
            preview_lines(app, *cursor, body.height.saturating_sub(1) as usize),
        ),
        Some(Modal::Link { input, .. }) => {
            let lines = vec![
                Line::from("   Paste a link from Discord's Copy Link, or a DM channel ID."),
                Line::styled("   https://discord.com/channels/@me/CHANNEL", dim()),
                Line::styled(
                    "   https://discord.com/channels/SERVER/CHANNEL/MESSAGE",
                    dim(),
                ),
                Line::from(""),
                field_line(true, "Link or ID", input_spans(input, true, false, "")),
            ];
            ("Add by link", lines)
        }
        Some(Modal::LinkRange(reference)) => {
            let option = |key: &str, text: &str| {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(pad(key, 8), accent().add_modifier(Modifier::BOLD)),
                    Span::raw(text.to_string()),
                ])
            };
            (
                "Use the linked message",
                vec![
                    Line::from(format!(
                        "   Message from {} in channel {}.",
                        snowflake_time(&reference.message_id),
                        reference.channel_id
                    )),
                    Line::from(""),
                    option("o", "Delete my messages older than it"),
                    option("n", "Delete my messages newer than it"),
                    option("Space", "Mark only this message"),
                    option("Esc", "Only add the channel"),
                ],
            )
        }
        _ => return,
    };
    frame.render_widget(title_row(name, true, Vec::new()), title);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        body.inner(Margin::new(0, u16::from(body.height > 12))),
    );
}

fn draw_welcome(frame: &mut Frame, area: Rect, app: &App) {
    let Some(Modal::Source(form)) = &app.modal else {
        return;
    };
    // A short screen drops the introduction, so the fields stay visible.
    let mut lines = if area.height >= 16 {
        vec![
            Line::styled("   Delete your own Discord messages, safely.", bold()),
            Line::styled(
                "   Every run starts as a preview. Nothing is deleted until you confirm.",
                dim(),
            ),
            Line::from(""),
        ]
    } else {
        Vec::new()
    };
    lines.extend(source_lines(app, form));
    let width = area.width.min(84);
    let height = wrapped_height(&lines, width.saturating_sub(4)) + 2;
    let rect = centered(area, width, height);
    let inner = card(frame, rect, "Welcome", accent());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let run = app.preview_ready();
    let label = |text: &str| Span::styled(pad(text, 9), dim());
    let filters = filter_parts(app);
    let lines = vec![
        Line::styled(
            format!(
                "Delete {} from {}?",
                plural(run.map_or(0, |r| r.total()), "message", "messages"),
                if app.package.is_some() {
                    "the account of your token".to_string()
                } else {
                    clean(&app.account)
                }
            ),
            danger().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from(vec![
            label("Scope"),
            Span::raw(scope_text(app).unwrap_or_default()),
        ]),
        Line::from(vec![label("Range"), Span::raw(range_text(app))]),
        Line::from(vec![
            label("Filters"),
            Span::raw(if filters.is_empty() {
                "none".to_string()
            } else {
                filters.join(" · ")
            }),
        ]),
        Line::from(""),
        Line::from("This cannot be undone. Only the previewed IDs are deleted."),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " y ",
                danger().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Span::raw(" delete    "),
            Span::styled("any other key cancels", dim()),
        ]),
    ];
    // A short screen drops the blank rows, so the y line stays inside.
    let lines: Vec<Line> = if area.height < 20 {
        lines.into_iter().filter(|l| l.width() > 0).collect()
    } else {
        lines
    };
    let width = area.width.min(68);
    let height = wrapped_height(&lines, width.saturating_sub(4)) + 2;
    let inner = card(
        frame,
        centered(area, width, height),
        "Delete for good",
        danger(),
    );
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn help_section(ctx: Ctx) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        ctx.title().to_uppercase(),
        dim().add_modifier(Modifier::BOLD),
    )];
    lines.extend(BINDINGS.iter().filter(|b| b.ctx == ctx).map(|b| {
        Line::from(vec![
            Span::styled(pad(b.key_label, 18), accent().add_modifier(Modifier::BOLD)),
            Span::raw(b.long),
        ])
    }));
    lines.push(Line::from(""));
    lines
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let mut left = help_section(Ctx::Where);
    left.extend(help_section(Ctx::Messages));
    left.extend([
        Line::styled("TEXT FIELDS", dim().add_modifier(Modifier::BOLD)),
        Line::from(vec![
            Span::styled(
                pad("← → Home End", 18),
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::raw("Move the cursor"),
        ]),
        Line::from(vec![
            Span::styled(
                pad("Ctrl-W Ctrl-U", 18),
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::raw("Delete a word / to the start"),
        ]),
    ]);
    let mut right = help_section(Ctx::Workspace);
    right.extend(help_section(Ctx::Run));
    right.extend(help_section(Ctx::Done));
    let two = area.width >= 150;
    let width = if two { 148 } else { area.width.min(96) };
    let height = if two {
        left.len().max(right.len())
    } else {
        left.len() + right.len()
    } as u16
        + 2;
    let inner = card(frame, centered(area, width, height), "Keys", accent());
    if two {
        let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(inner);
        frame.render_widget(Paragraph::new(left), l);
        frame.render_widget(Paragraph::new(right), r);
    } else {
        // The pane keys are the most needed, so they go first.
        left.extend(right);
        frame.render_widget(Paragraph::new(left), inner);
    }
}

fn draw_palette(frame: &mut Frame, area: Rect, app: &App, input: &TextInput, cursor: usize) {
    let items = app.palette_items(&input.text);
    let width = area.width.saturating_sub(4).min(76);
    let rows = (items.len() as u16).clamp(1, 12);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + 2,
        width,
        height: (rows + 4).min(area.height.saturating_sub(3)),
    };
    let inner = card(frame, rect, "Commands", accent());
    let [query, _, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);
    let mut spans = vec![Span::styled("› ", accent().add_modifier(Modifier::BOLD))];
    spans.extend(input_spans(input, true, false, "type to find an action"));
    frame.render_widget(Line::from(spans), query);
    if items.is_empty() {
        frame.render_widget(Line::styled("No action matches.", dim()), list);
        return;
    }
    let lines: Vec<ListItem> = items
        .iter()
        .map(|b| {
            ListItem::new(Line::from(vec![
                Span::styled(pad(b.key_label, 10), accent()),
                Span::raw(b.long),
            ]))
        })
        .collect();
    render_list(frame, list, lines, cursor.min(items.len() - 1), true);
}

fn phase_style(phase: &RunPhase) -> Style {
    match phase {
        RunPhase::Running => accent(),
        RunPhase::Completed => ok(),
        RunPhase::Aborting(_) | RunPhase::Aborted(_) => danger(),
        _ => info(),
    }
}

fn count_spans(snapshot: &Snapshot) -> Vec<Span<'static>> {
    let count = |value: i64, label: &'static str, bad: bool| {
        let style = if bad && value > 0 { danger() } else { bold() };
        [
            Span::styled(value.to_string(), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {label}    "), dim()),
        ]
    };
    let mut spans = vec![Span::raw("  ")];
    spans.extend(count(snapshot.deleted, "deleted", false));
    spans.extend(count(snapshot.skipped, "skipped", false));
    spans.extend(count(snapshot.deferred, "deferred", true));
    spans.extend(count(snapshot.failed, "failed", true));
    spans
}

fn draw_run(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let area = area.inner(Margin::new(1, 1));
    let [phase, gauge, numbers, counts, _, rate, _, detail] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(area);
    let style = phase_style(&snapshot.phase);
    let mut head = vec![Span::styled(
        format!(" {} ", snapshot.phase.label()),
        style.add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )];
    if snapshot.phase == RunPhase::Running {
        head.push(Span::styled(format!(" {}", spinner(app)), accent()));
    }
    if let Some(reason) = snapshot.phase.reason() {
        head.push(Span::styled(format!("  {}", clean(reason)), danger()));
    } else if !snapshot.status.is_empty() {
        head.push(Span::styled(
            format!("  {}", clean(&snapshot.status)),
            info(),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(head)).wrap(Wrap { trim: true }),
        phase,
    );
    let ratio = if snapshot.total > 0 {
        (snapshot.processed as f64 / snapshot.total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    frame.render_widget(
        LineGauge::default()
            .filled_style(style)
            .unfilled_style(dim())
            .line_set(symbols::line::THICK)
            .label(Span::styled(format!("{:>4.0}%", ratio * 100.0), bold()))
            .ratio(ratio),
        gauge,
    );
    let eta = snapshot
        .eta
        .map(format_duration)
        .unwrap_or_else(|| "—".into());
    frame.render_widget(
        Line::styled(
            format!(
                "  {} of {} processed · {:.2} per second · {} elapsed · {} left",
                snapshot.processed,
                snapshot.total,
                snapshot.rate,
                format_duration(snapshot.elapsed),
                eta
            ),
            dim(),
        ),
        numbers,
    );
    frame.render_widget(Line::from(count_spans(snapshot)), counts);
    let [rate_label, rate_chart] =
        Layout::horizontal([Constraint::Length(14), Constraint::Min(1)]).areas(rate);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("  RATE", dim().add_modifier(Modifier::BOLD)),
            Line::styled(format!("  avg {:.2}/s", snapshot.rate), dim()),
            Line::styled("  each second", dim()),
        ]),
        rate_label,
    );
    let start = app.rates.len().saturating_sub(rate_chart.width as usize);
    frame.render_widget(
        Sparkline::default().data(&app.rates[start..]).style(style),
        rate_chart,
    );
    let mut lines = vec![Line::styled(
        "  CHANNELS",
        dim().add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        snapshot
            .limit_notice()
            .map(|n| Line::styled(format!("  {n}"), accent())),
    );
    for worker in snapshot.workers.iter().filter(|w| w.active) {
        let bar = (worker.done * 16)
            .checked_div(worker.total)
            .unwrap_or(0)
            .min(16);
        lines.push(Line::from(vec![
            Span::styled("  ▸ ", accent()),
            Span::styled("━".repeat(bar), style),
            Span::styled("─".repeat(16 - bar), dim()),
            Span::styled(format!(" {:>5}/{:<5} ", worker.done, worker.total), dim()),
            Span::raw(clean(&worker.channel)),
        ]));
    }
    detail_notes(&mut lines, app, snapshot);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), detail);
}

/// Errors, skip reasons, and the app notice, under the run or its summary.
fn detail_notes(lines: &mut Vec<Line<'static>>, app: &App, snapshot: &Snapshot) {
    for error in snapshot.errors.iter().rev().take(6) {
        lines.push(Line::styled(format!("  ✗ {}", clean(error)), danger()));
    }
    for failure in snapshot.forbidden.iter().take(6) {
        if !snapshot.errors.iter().any(|e| e.contains(&failure.reason)) {
            lines.push(Line::styled(
                format!(
                    "  {}: {} ({})",
                    clean(&failure.label),
                    clean(&failure.reason),
                    plural(failure.count, "message", "messages")
                ),
                accent(),
            ));
        }
    }
    if let Some(notice) = &app.notice {
        lines.push(Line::styled(format!("  {}", clean(notice)), dim()));
    }
}

fn draw_done(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let area = area.inner(Margin::new(1, 1));
    let (icon, style) = match &snapshot.phase {
        RunPhase::Completed if snapshot.complete() => ("✓", ok()),
        RunPhase::Completed => ("!", accent()),
        RunPhase::Aborted(_) | RunPhase::Aborting(_) => ("✗", danger()),
        _ => ("■", info()),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {icon} {} ", snapshot.phase.label()),
            style.add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::styled(
            format!(
                "  {} of {} processed in {}",
                snapshot.processed,
                snapshot.total,
                format_duration(snapshot.elapsed)
            ),
            dim(),
        ),
    ])];
    if let Some(reason) = snapshot.phase.reason() {
        lines.push(Line::styled(format!("  {}", clean(reason)), danger()));
    }
    lines.push(Line::from(""));
    let tile = |value: i64, label: &str, meaning: &str, bad: bool| {
        let value_style = if bad && value > 0 { danger() } else { bold() };
        Line::from(vec![
            Span::styled(
                format!("  {:>8}  ", value),
                value_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(pad(label, 10), bold()),
            Span::styled(meaning.to_string(), dim()),
        ])
    };
    lines.extend([
        tile(snapshot.deleted, "deleted", "The message is gone.", false),
        tile(
            snapshot.skipped,
            "skipped",
            "No retry can delete it: a system message or one you keep.",
            false,
        ),
        tile(
            snapshot.deferred,
            "deferred",
            "A later run can delete it. Run again to continue.",
            true,
        ),
        tile(
            snapshot.failed,
            "failed",
            "Discord returned an error. Read the report, then retry.",
            true,
        ),
        Line::from(""),
    ]);
    if let Screen::Done { report, .. } = &app.screen {
        lines.push(Line::from(vec![
            Span::styled("  Report    ", dim()),
            Span::styled(clean(report), info()),
        ]));
        lines.push(Line::from(""));
    }
    detail_notes(&mut lines, app, snapshot);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{History, Screen};
    use discord_purge_core::Options;
    use discord_purge_core::api::{Channel, DiscordClient};
    use discord_purge_core::engine::{Engine, ForbiddenStat, RunPhase, Stats};
    use discord_purge_core::filter::Filter;

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A run screen with controls that send nothing to Discord.
    fn running(total: usize) -> Screen {
        let client = DiscordClient::with_base("token", "http://127.0.0.1:9").unwrap();
        Screen::Running(RunHandle {
            controls: Engine::new(Default::default(), client, total).1,
            task: tokio::spawn(std::future::pending()),
        })
    }

    fn empty() -> App {
        App::new(Options {
            token: None,
            package: None,
            guild: None,
            channel: None,
            settings: Default::default(),
            filter: Filter::default(),
            include_pinned: false,
            token_store: discord_purge_core::resume::TokenStore::Off,
        })
    }

    #[test]
    fn workspace_shows_where_messages_and_the_plan_on_one_screen() {
        let app = empty();
        let screen = render(&app, 100, 30);
        for text in ["Where", "Messages", "Plan", "Press p", "? keys"] {
            assert!(screen.contains(text), "missing {text:?} in\n{screen}");
        }
        assert!(render(&app, MIN_WIDTH - 1, 30).contains("Enlarge the terminal"));
    }

    #[test]
    fn a_narrow_terminal_shows_the_focused_pane_and_the_footer_keeps_help() {
        let app = empty();
        let screen = render(&app, 60, 18);
        assert!(screen.contains("Where") && !screen.contains("Messages"));
        assert!(screen.contains("? keys"), "missing help key in\n{screen}");
    }

    #[tokio::test]
    async fn running_screen_shows_progress_and_channel_skip_reason() {
        let mut app = empty();
        app.screen = running(31);
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Deleting") && screen.contains("0 of 31"));
        let mut snapshot = Stats::new(31, 1).snapshot();
        snapshot.forbidden.push(ForbiddenStat {
            count: 1,
            reason: "Only bots can use this endpoint".into(),
            label: "test channel".into(),
        });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw_run(frame, frame.area(), &app, &snapshot))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            rendered.contains("Only bots can use this endpoint"),
            "a forbidden response must be visible even with an empty status string"
        );
    }

    #[test]
    fn running_screen_shows_a_pause_and_an_abort_reason() {
        let app = empty();
        let mut snapshot = Stats::new(1, 1).snapshot();
        snapshot.phase = RunPhase::Paused;
        let rendered = |snapshot: &Snapshot| {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| draw_run(frame, frame.area(), &app, snapshot))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(rendered(&snapshot).contains("Paused"));
        snapshot.phase = RunPhase::Aborted("Discord answered 401".into());
        let screen = rendered(&snapshot);
        assert!(screen.contains("Aborted") && screen.contains("Discord answered 401"));
    }

    #[test]
    fn running_screen_shows_a_reduced_request_limit() {
        let app = empty();
        let mut snapshot = Stats::new(1, 4).snapshot();
        snapshot.active_limit = 1;
        // Wide enough that the notice does not wrap inside the checked words.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| draw_run(frame, frame.area(), &app, &snapshot))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("one request at a time"));
    }

    #[test]
    fn a_failed_first_history_load_shows_the_reason_not_an_empty_channel() {
        let mut app = empty();
        app.history = Some(History {
            channel: Channel {
                id: "1".into(),
                name: Some("general".into()),
                kind: 0,
                guild_id: None,
                recipients: Vec::new(),
            },
            messages: Vec::new(),
            more: false,
            load: HistoryLoad::Failed("Missing Access.".into()),
            cursor: 0,
        });
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Missing Access") && !screen.contains("no messages to show"));
    }
}
