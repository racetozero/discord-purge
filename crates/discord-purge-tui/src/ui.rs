//! Drawing for the terminal workspace, the run, and the modals.
use discord_purge_core::engine::Snapshot;
use discord_purge_core::filter::{ContentMatch, snowflake_time as snowflake_created};
use discord_purge_core::resume::format_duration;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::app::{App, Focus, Modal, Range, Row, Screen, TYPES, message_ref};

/// Highlighter amber marks what is selected; red is only for deletion.
const MARK: Color = Color::Rgb(242, 183, 5);
const DANGER: Color = Color::Rgb(232, 72, 56);
const MUTED: Color = Color::DarkGray;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(crate) const MIN_WIDTH: u16 = 60;
pub(crate) const MIN_HEIGHT: u16 = 18;

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|channel| if channel.is_control() { ' ' } else { channel })
        .collect()
}

fn pane(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { MARK } else { MUTED }))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

fn hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, action) in keys {
        spans.push(Span::styled(
            format!(" {key}"),
            Style::default().fg(MARK).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {action} "),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
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
    let [header, body, plan, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(frame, header, app);
    match app.screen {
        Screen::Workspace => {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
                    .areas(body);
            draw_where(frame, left, app);
            draw_messages(frame, right, app);
            draw_plan(frame, plan, app);
            frame.render_widget(workspace_hints(app), footer);
            if let Some(modal) = &app.modal {
                draw_modal(frame, area, app, modal);
            }
        }
        Screen::Running | Screen::Done => {
            let [run] = Layout::vertical([Constraint::Min(5)]).areas(body.union(plan));
            if let Some(stats) = &app.stats {
                draw_run(frame, run, app, &stats.snapshot());
            }
            frame.render_widget(
                if app.screen == Screen::Running {
                    hints(&[("Space", "pause/continue"), ("s", "stop")])
                } else {
                    hints(&[("Enter", "new selection"), ("q", "quit")])
                },
                footer,
            );
        }
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let (who, how) = match &app.package {
        Some(path) => (
            format!(
                "Data package {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            if app.token.is_some() {
                "token ready for deletion"
            } else {
                "preview only until you add a token"
            },
        ),
        None if app.account.is_empty() => ("Not signed in".to_string(), "press s to start"),
        None => (clean(&app.account), "Discord account"),
    };
    let mut spans = vec![
        Span::styled(
            " discord-purge ",
            Style::default()
                .fg(Color::Black)
                .bg(MARK)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(who, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {how}"), Style::default().fg(MUTED)),
    ];
    if app.busy() {
        spans.push(Span::styled(
            format!("  {} {}…", SPINNER[app.frame % SPINNER.len()], app.activity),
            Style::default().fg(MARK),
        ));
    }
    f.render_widget(Line::from(spans), area);
}

fn mark(on: bool, partial: bool) -> &'static str {
    match (on, partial) {
        (true, _) => "[x]",
        (false, true) => "[~]",
        _ => "[ ]",
    }
}

fn draw_where(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Where && app.modal.is_none();
    let title = if app.typing_query || !app.query.is_empty() {
        format!(
            " Where · /{}{} ",
            app.query,
            if app.typing_query { "▏" } else { "" }
        )
    } else {
        " Where ".into()
    };
    let block = pane(title, focused);
    if !app.listed {
        let text = if app.busy() {
            "Loading your servers and direct messages…"
        } else {
            "No lists are loaded. Press s to sign in with a token or to open a data package."
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(MUTED))
                .block(block),
            area,
        );
        return;
    }
    let rows = app.rows();
    let picked = |on: bool| {
        if on {
            Style::default().fg(MARK)
        } else {
            Style::default()
        }
    };
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            Row::ServersHeader => {
                let chosen = app
                    .servers
                    .iter()
                    .filter(|s| s.all || !s.picked.is_empty())
                    .count();
                ListItem::new(Line::from(vec![
                    Span::styled("Servers", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("  {chosen} of {}", app.servers.len()),
                        Style::default().fg(MUTED),
                    ),
                ]))
            }
            Row::Server(index) => {
                let s = &app.servers[index];
                let arrow = if s.expanded { "▾" } else { "▸" };
                let tail = if s.loading { " …" } else { "" };
                ListItem::new(format!(
                    "{arrow} {} {}{tail}",
                    mark(s.all, !s.picked.is_empty()),
                    clean(&s.name)
                ))
                .style(picked(s.all || !s.picked.is_empty()))
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
                let on = s.all || s.picked.contains(&c.id);
                let name = c.label();
                let name = if c.guild_id.is_some() && !name.starts_with('#') {
                    format!("#{name}")
                } else {
                    name
                };
                ListItem::new(format!("    {} {}", mark(on, false), clean(&name))).style(picked(on))
            }
            Row::DmsHeader => {
                let chosen = app.dms.iter().filter(|dm| dm.checked).count();
                ListItem::new(Line::from(vec![
                    Span::styled(
                        "Direct messages",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {chosen} of {}", app.dms.len()),
                        Style::default().fg(MUTED),
                    ),
                ]))
            }
            Row::Dm(index) => {
                let dm = &app.dms[index];
                ListItem::new(format!(
                    "  {} {}",
                    mark(dm.checked, false),
                    clean(&dm.channel.label())
                ))
                .style(picked(dm.checked))
            }
        })
        .collect();
    let mut state =
        ListState::default().with_selected(Some(app.cursor.min(rows.len().saturating_sub(1))));
    frame.render_stateful_widget(
        List::new(items).block(block).highlight_style(if focused {
            Style::default()
                .bg(Color::Rgb(58, 52, 30))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(40, 40, 40))
        }),
        area,
        &mut state,
    );
}

fn draw_messages(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Messages && app.modal.is_none();
    let Some(history) = &app.history else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Open a channel or DM with Enter or → to read its messages here."),
                Line::from(""),
                Line::from("Space marks single messages to delete."),
                Line::from("o deletes everything older than a message; n everything newer."),
                Line::from("Without either, the plan deletes all your messages in the scope."),
            ])
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(MUTED))
            .block(pane(" Messages ".into(), focused)),
            area,
        );
        return;
    };
    let title = format!(" Messages · {} ", clean(&history.channel.label()));
    if history.messages.is_empty() {
        let text = if history.loading {
            "Loading messages…"
        } else {
            "This channel has no messages to show."
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(MUTED))
                .block(pane(title, focused)),
            area,
        );
        return;
    }
    let anchor = match &app.range {
        Range::Older(a) | Range::Newer(a) => Some(a),
        _ => None,
    };
    let history_in_scope = app.history_in_scope();
    let mut items: Vec<ListItem> = history
        .messages
        .iter()
        .map(|message| {
            let reference = message_ref(message);
            let deleted = app.in_range(message, history_in_scope);
            let (marker, marker_style) = if anchor == Some(&reference) {
                ("◆ ", Style::default().fg(MARK).add_modifier(Modifier::BOLD))
            } else if matches!(&app.range, Range::Only(set) if set.contains(&reference)) {
                ("● ", Style::default().fg(MARK))
            } else if app.own(message) {
                ("· ", Style::default().fg(MUTED))
            } else {
                ("  ", Style::default())
            };
            let mut text = clean(&message.content);
            if text.trim().is_empty() {
                text = match message.attachments.first() {
                    Some(a) => format!("[{}]", clean(&a.filename)),
                    None => "[no text]".into(),
                };
            }
            if message.pinned {
                text.push_str("  [pinned]");
            }
            let body = if deleted {
                Style::default()
                    .fg(MUTED)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(
                    format!("{} ", snowflake_time(&message.id)),
                    Style::default().fg(MUTED),
                ),
                Span::styled(
                    format!(
                        "{}: ",
                        clean(
                            &message
                                .author
                                .global_name
                                .clone()
                                .unwrap_or(message.author.username.clone())
                        )
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(text, body),
            ]))
        })
        .collect();
    items.push(ListItem::new(Span::styled(
        if history.loading {
            "  Loading older messages…"
        } else if history.more {
            "  ↓ Move down to load older messages"
        } else {
            "  Start of the history"
        },
        Style::default().fg(MUTED),
    )));
    let mut state = ListState::default().with_selected(Some(history.cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane(title, focused))
            .highlight_style(if focused {
                Style::default().bg(Color::Rgb(58, 52, 30))
            } else {
                Style::default().bg(Color::Rgb(40, 40, 40))
            }),
        area,
        &mut state,
    );
}

fn scope_text(app: &App) -> String {
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
    if parts.is_empty() {
        "Nothing selected. Press Space on a server, channel, or DM.".into()
    } else {
        parts.join(", ")
    }
}

fn range_text(app: &App) -> String {
    match &app.range {
        Range::All => "All your messages in the scope".into(),
        Range::Older(a) => format!(
            "Your messages older than the one from {} (that one stays)",
            snowflake_time(&a.message_id)
        ),
        Range::Newer(a) => format!(
            "Your messages newer than the one from {} (that one stays)",
            snowflake_time(&a.message_id)
        ),
        Range::Only(set) if set.is_empty() => {
            "No messages marked. Press Space on a message.".into()
        }
        Range::Only(set) => format!(
            "Only {}",
            plural(set.len(), "marked message", "marked messages")
        ),
    }
}

fn filter_text(app: &App) -> String {
    let frame = &app.config.filter;
    let inputs = frame.inputs();
    let mut parts = Vec::new();
    match &frame.content {
        ContentMatch::Any => {}
        _ => parts.push(format!("text {:?}", inputs.content)),
    }
    if !inputs.types.is_empty() {
        parts.push(inputs.types.replace(',', "+"));
    }
    if !inputs.after_date.is_empty() || !inputs.before_date.is_empty() {
        parts.push(format!("{} to {}", inputs.after_date, inputs.before_date));
    }
    if app.config.include_pinned {
        parts.push("pinned included".into());
    }
    let s = &app.config.settings;
    format!(
        "{}  ·  {} workers, {:.1}s ±{:.0}%",
        if parts.is_empty() {
            "no filters".into()
        } else {
            parts.join(", ")
        },
        s.workers,
        s.delay.as_secs_f64(),
        s.jitter * 100.0
    )
}

fn draw_plan(frame: &mut Frame, area: Rect, app: &App) {
    let label = |s: &'static str| Span::styled(format!("{s:<9}"), Style::default().fg(MUTED));
    let status = if let Some(error) = &app.error {
        Line::styled(clean(error), Style::default().fg(DANGER))
    } else if let Some(notice) = &app.notice {
        Line::styled(clean(notice), Style::default().fg(Color::Cyan))
    } else if let Some(run) = app.preview_ready() {
        Line::from(vec![
            Span::styled(
                format!(
                    "Preview: {} in {}. ",
                    plural(run.total, "message", "messages"),
                    plural(run.jobs.len(), "channel", "channels")
                ),
                Style::default().fg(MARK).add_modifier(Modifier::BOLD),
            ),
            Span::raw("Press d to delete them."),
        ])
    } else if app.preview.is_some() {
        Line::styled(
            "The selection changed after the preview. Press p to count again.",
            Style::default().fg(MARK),
        )
    } else {
        Line::styled(
            "Press p to count the exact messages. Nothing is deleted until you confirm.",
            Style::default().fg(MUTED),
        )
    };
    let lines = vec![
        Line::from(vec![label("Scope"), Span::raw(scope_text(app))]),
        Line::from(vec![label("Range"), Span::raw(range_text(app))]),
        Line::from(vec![label("Filters"), Span::raw(filter_text(app))]),
        status,
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(pane(" Plan ".into(), false)),
        area,
    );
}

fn workspace_hints(app: &App) -> Line<'static> {
    if app.typing_query {
        return hints(&[
            ("type", "filter by name"),
            ("Enter", "keep"),
            ("Esc", "clear"),
        ]);
    }
    match app.focus {
        Focus::Where => hints(&[
            ("Space", "select"),
            ("→", "open"),
            ("/", "find"),
            ("p", "preview"),
            ("d", "delete"),
            ("f", "filters"),
            ("?", "help"),
            ("q", "quit"),
        ]),
        Focus::Messages => hints(&[
            ("Space", "mark"),
            ("o", "older than"),
            ("n", "newer than"),
            ("c", "clear"),
            ("←", "back"),
            ("p", "preview"),
            ("d", "delete"),
        ]),
    }
}

fn draw_run(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let [gauge, numbers, status, detail] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(3),
    ])
    .areas(area);
    let ratio = if snapshot.total > 0 {
        (snapshot.processed as f64 / snapshot.total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let title = match (app.screen, snapshot.aborted) {
        (Screen::Done, true) => " Stopped ",
        (Screen::Done, false) => " Finished ",
        _ => " Deleting ",
    };
    frame.render_widget(
        Gauge::default()
            .block(pane(title.into(), true))
            .gauge_style(Style::default().fg(MARK).bg(Color::Rgb(40, 40, 40)))
            .ratio(ratio)
            .label(format!("{} of {}", snapshot.processed, snapshot.total)),
        gauge,
    );
    let eta = snapshot
        .eta
        .map(format_duration)
        .unwrap_or_else(|| "—".into());
    let number = |value: String, label: &'static str, color: Color| {
        [
            Span::styled(
                value,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {label}   "), Style::default().fg(MUTED)),
        ]
    };
    let spans: Vec<Span> = [
        number(snapshot.deleted.to_string(), "deleted", MARK),
        number(snapshot.skipped.to_string(), "skipped", Color::Reset),
        number(
            snapshot.failed.to_string(),
            "failed",
            if snapshot.failed > 0 {
                DANGER
            } else {
                Color::Reset
            },
        ),
        number(format!("{:.2}", snapshot.rate), "per second", Color::Reset),
        number(format_duration(snapshot.elapsed), "elapsed", Color::Reset),
        number(eta, "left", Color::Reset),
    ]
    .into_iter()
    .flatten()
    .collect();
    frame.render_widget(Line::from(spans), numbers);
    let mut text = clean(&snapshot.status);
    if app.screen == Screen::Done {
        text = format!("{text}  Report: {}", app.report);
    }
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::Cyan)),
        status,
    );
    let mut lines: Vec<Line> = snapshot
        .workers
        .iter()
        .filter(|w| w.active)
        .map(|w| {
            Line::from(vec![
                Span::styled("▸ ", Style::default().fg(MARK)),
                Span::raw(clean(&w.channel)),
                Span::styled(
                    format!("  {}/{}", w.done, w.total),
                    Style::default().fg(MUTED),
                ),
            ])
        })
        .collect();
    for error in snapshot.errors.iter().rev().take(6) {
        lines.push(Line::styled(
            format!("! {}", clean(error)),
            Style::default().fg(DANGER),
        ));
    }
    for failure in snapshot.forbidden.iter().take(6) {
        if !snapshot.errors.iter().any(|e| e.contains(&failure.reason)) {
            lines.push(Line::styled(
                format!(
                    "{}: {} ({})",
                    clean(&failure.label),
                    clean(&failure.reason),
                    plural(failure.count, "message", "messages")
                ),
                Style::default().fg(MARK),
            ));
        }
    }
    if let Some(notice) = &app.notice {
        lines.push(Line::styled(clean(notice), Style::default().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(pane(" Channels and skip reasons ".into(), false)),
        detail,
    );
}

fn field_line(active: bool, label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if active { "› " } else { "  " },
            Style::default().fg(MARK).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{label:<14}"), Style::default().fg(MUTED)),
        value,
    ])
}

fn input(text: &str, active: bool) -> Span<'static> {
    Span::styled(
        format!("{}{}", clean(text), if active { "▏" } else { "" }),
        if active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        },
    )
}

fn draw_modal(frame: &mut Frame, area: Rect, app: &App, modal: &Modal) {
    let (title, width, lines, danger): (String, u16, Vec<Line>, bool) = match modal {
        Modal::Help => (
            " Keys ".into(),
            72,
            [
                ("Tab", "Switch between Where and Messages"),
                ("↑ ↓ PgUp PgDn", "Move"),
                ("Space", "Select a server, channel, or DM; mark a message"),
                ("→ / Enter", "Expand a server; open a channel's messages"),
                ("←", "Collapse, or go back to Where"),
                ("/", "Find servers, channels, and DMs by name"),
                ("o / n", "Delete messages older / newer than this one"),
                ("c", "Clear the range: all your messages"),
                ("l", "Paste a Discord link or DM channel ID"),
                ("f", "Filters and speed"),
                ("s", "Source: account token or data package"),
                ("r", "Reload the lists"),
                ("p", "Preview the exact messages (deletes nothing)"),
                ("d", "Delete the previewed messages, after y"),
                ("Esc", "Close, or cancel a running preview"),
                ("q", "Quit"),
            ]
            .into_iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(
                        format!("{k:<15}"),
                        Style::default().fg(MARK).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(v),
                ])
            })
            .collect(),
            false,
        ),
        Modal::Confirm => {
            let run = app.preview_ready();
            (
                " Delete for good ".into(),
                64,
                vec![
                    Line::styled(
                        format!(
                            "Delete {} from {}?",
                            plural(run.map_or(0, |r| r.total), "message", "messages"),
                            if app.package.is_some() {
                                "the account of your token".to_string()
                            } else {
                                clean(&app.account)
                            }
                        ),
                        Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                    ),
                    Line::from(""),
                    Line::from(format!("Scope: {}", scope_text(app))),
                    Line::from(format!("Range: {}", range_text(app))),
                    Line::from(""),
                    Line::from("This cannot be undone. Only the previewed IDs are deleted."),
                    Line::from(vec![
                        Span::styled(
                            "y",
                            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" delete    any other key cancels"),
                    ]),
                ],
                true,
            )
        }
        Modal::Preview { cursor } => {
            let mut lines = Vec::new();
            if let Some(run) = app.preview_ready() {
                lines.push(Line::from(vec![
                    Span::styled(
                        plural(run.total, "message", "messages"),
                        Style::default().fg(MARK).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " in {}. Nothing has been deleted.",
                        plural(run.jobs.len(), "channel", "channels")
                    )),
                ]));
                if app.package.is_some() {
                    lines.push(Line::styled(
                        "A package cannot show pins or access, so this is a maximum.",
                        Style::default().fg(MUTED),
                    ));
                }
                lines.push(Line::from(""));
                let shown = 12usize;
                let start = cursor.saturating_sub(shown - 1);
                for (index, job) in run.jobs.iter().enumerate().skip(start).take(shown) {
                    let style = if index == *cursor {
                        Style::default().bg(Color::Rgb(58, 52, 30))
                    } else {
                        Style::default()
                    };
                    lines.push(Line::styled(
                        format!("{:>8}  {}", job.items.len(), clean(&job.label)),
                        style,
                    ));
                }
                lines.push(Line::from(""));
            }
            lines.push(hints(&[
                ("d", "delete these"),
                ("p", "count again"),
                ("Esc", "close"),
            ]));
            (" Exact preview ".into(), 70, lines, false)
        }
        Modal::Source(form) => {
            let mode = |on: bool, label: &str| {
                Span::styled(
                    format!("{} {label}   ", if on { "●" } else { "○" }),
                    if on {
                        Style::default().fg(MARK).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                )
            };
            let token = if form.token.is_empty() && app.token.is_some() {
                Span::styled("saved token in use", Style::default().fg(MUTED))
            } else {
                input(&"•".repeat(form.token.chars().count()), form.field == 1)
            };
            let mut lines = Vec::new();
            if form.for_delete {
                lines.push(Line::from(
                    "Deletion needs your account token. The preview stays as it is.",
                ));
            } else {
                let mut source = field_line(form.field == 0, "Source", Span::from(""));
                source.spans.extend([
                    mode(!form.package_mode, "Discord account"),
                    mode(form.package_mode, "Data package"),
                ]);
                lines.push(source);
            }
            lines.push(field_line(form.field == 1, "Token", token));
            if !form.for_delete {
                lines.push(field_line(
                    form.field == 2,
                    "Package path",
                    input(&form.package, form.field == 2),
                ));
            }
            if let Some(error) = &form.error {
                lines.push(Line::styled(clean(error), Style::default().fg(DANGER)));
            }
            lines.push(Line::from(""));
            lines.extend(
                discord_purge_core::TOKEN_HELP
                    .lines()
                    .map(|l| Line::styled(l.to_string(), Style::default().fg(MUTED))),
            );
            lines.push(Line::from(""));
            lines.push(hints(&[
                ("↑↓", "field"),
                ("←→", "source"),
                ("Enter", "continue"),
                ("Esc", "close"),
            ]));
            (" Source ".into(), 80, lines, false)
        }
        Modal::Filters(form) => {
            let chips: Vec<Span> = TYPES
                .iter()
                .enumerate()
                .map(|(index, t)| {
                    let mut style = if form.types[index] {
                        Style::default().fg(MARK).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    };
                    if form.field == 1 && form.type_cursor == index {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    Span::styled(
                        format!("{}{t} ", if form.types[index] { "+" } else { " " }),
                        style,
                    )
                })
                .collect();
            let mut lines = vec![
                field_line(
                    form.field == 0,
                    "Text",
                    input(&form.content, form.field == 0),
                ),
                field_line(form.field == 1, "Types", Span::from("")),
                field_line(
                    form.field == 2,
                    "From date",
                    input(&form.after, form.field == 2),
                ),
                field_line(
                    form.field == 3,
                    "To date",
                    input(&form.before, form.field == 3),
                ),
                field_line(
                    form.field == 4,
                    "Pinned",
                    Span::raw(if form.pinned {
                        "[x] delete pinned messages too"
                    } else {
                        "[ ] keep pinned messages"
                    }),
                ),
                field_line(
                    form.field == 5,
                    "Workers",
                    input(&form.workers, form.field == 5),
                ),
                field_line(
                    form.field == 6,
                    "Delay (s)",
                    input(&form.delay, form.field == 6),
                ),
                field_line(
                    form.field == 7,
                    "Jitter",
                    input(&form.jitter, form.field == 7),
                ),
            ];
            lines[1].spans.extend(chips);
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Text: words to find, or /regex/ or /regex/i. Dates: YYYY-MM-DD in UTC.",
                Style::default().fg(MUTED),
            ));
            lines.push(Line::styled(
                "No type selected means every type. Workers: 1-16 channels at a time.",
                Style::default().fg(MUTED),
            ));
            if let Some(error) = &form.error {
                lines.push(Line::styled(clean(error), Style::default().fg(DANGER)));
            }
            lines.push(Line::from(""));
            lines.push(hints(&[
                ("↑↓", "field"),
                ("←→ Space", "types"),
                ("Enter", "save"),
                ("Esc", "cancel"),
            ]));
            (" Filters and speed ".into(), 78, lines, false)
        }
        Modal::Link { input: text, error } => {
            let mut lines = vec![
                Line::from("Paste a link from Discord's Copy Link, or a DM channel ID."),
                Line::styled(
                    "https://discord.com/channels/@me/CHANNEL  or  …/channels/SERVER/CHANNEL/MESSAGE",
                    Style::default().fg(MUTED),
                ),
                Line::from(""),
                field_line(true, "Link or ID", input(text, true)),
            ];
            if let Some(error) = error {
                lines.push(Line::styled(clean(error), Style::default().fg(DANGER)));
            }
            lines.push(Line::from(""));
            lines.push(hints(&[("Enter", "add to scope"), ("Esc", "close")]));
            (" Add by link ".into(), 84, lines, false)
        }
        Modal::LinkRange(reference) => (
            " Use the linked message ".into(),
            64,
            vec![
                Line::from(format!(
                    "Message from {} in channel {}:",
                    snowflake_time(&reference.message_id),
                    reference.channel_id
                )),
                Line::from(""),
                hints(&[("o", "delete my messages older than it")]),
                hints(&[("n", "delete my messages newer than it")]),
                hints(&[("Space", "mark only this message")]),
                hints(&[("Esc", "only add the channel")]),
            ],
            false,
        ),
    };
    let inner = width
        .min(area.width.saturating_sub(2))
        .saturating_sub(2)
        .max(1) as usize;
    let height = lines
        .iter()
        .map(|l| l.width().div_ceil(inner).max(1) as u16)
        .sum::<u16>()
        + 2;
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block =
        pane(title, true).border_style(Style::default().fg(if danger { DANGER } else { MARK }));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use discord_purge_core::Options;
    use discord_purge_core::engine::{ForbiddenStat, Stats};
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
    fn workspace_shows_all_three_decisions_on_one_screen() {
        let app = empty();
        let screen = render(&app, 100, 30);
        for text in [
            "Where", "Messages", "Plan", "Scope", "Range", "Filters", "Press p",
        ] {
            assert!(screen.contains(text), "missing {text:?} in\n{screen}");
        }
        assert!(render(&app, 59, 30).contains("Enlarge the terminal"));
    }

    #[test]
    fn running_screen_shows_progress_and_channel_skip_reason() {
        let mut app = empty();
        app.screen = Screen::Running;
        app.stats = Some(Stats::new(31, 1));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Deleting") && screen.contains("0 of 31"));
        let mut snapshot = app.stats.as_ref().unwrap().snapshot();
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
            .map(|channel| channel.symbol())
            .collect();
        assert!(
            rendered.contains("Only bots can use this endpoint"),
            "a forbidden response must be visible even with an empty status string"
        );
    }
}
