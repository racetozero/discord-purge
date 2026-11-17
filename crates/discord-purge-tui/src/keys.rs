//! The one table of key bindings. Dispatch, the footer, the key overlay, and
//! the command palette all read it, so a key cannot work without being shown,
//! or be shown without working.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    Quit,
    Help,
    Palette,
    Source,
    Filters,
    Link,
    Reload,
    Preview,
    Delete,
    SwitchPane,
    Find,
    Move,
    Select,
    Open,
    Back,
    Mark,
    Older,
    Newer,
    ClearRange,
    PauseResume,
    Stop,
    NewSelection,
}

/// Where a binding works. `Workspace` bindings work in both panes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ctx {
    Workspace,
    Where,
    Messages,
    Run,
    Done,
}

impl Ctx {
    pub fn title(self) -> &'static str {
        match self {
            Ctx::Workspace => "Anywhere",
            Ctx::Where => "Where",
            Ctx::Messages => "Messages",
            Ctx::Run => "During a run",
            Ctx::Done => "After a run",
        }
    }
}

pub(crate) struct Binding {
    pub action: Action,
    pub ctx: Ctx,
    pub keys: &'static [(KeyCode, KeyModifiers)],
    /// The keys as the footer and the key overlay show them.
    pub key_label: &'static str,
    /// A short label for the footer.
    pub short: &'static str,
    /// A full description for the key overlay and the palette.
    pub long: &'static str,
    /// Footer priority: lower shows first; `None` keeps it out of the footer.
    pub footer: Option<u8>,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

const fn key(c: char) -> (KeyCode, KeyModifiers) {
    (KeyCode::Char(c), NONE)
}

pub(crate) const BINDINGS: &[Binding] = &[
    Binding {
        action: Action::Select,
        ctx: Ctx::Where,
        keys: &[key(' ')],
        key_label: "Space",
        short: "select",
        long: "Select a server, channel, or DM. On a header, select every row in it",
        footer: Some(0),
    },
    Binding {
        action: Action::Open,
        ctx: Ctx::Where,
        keys: &[(KeyCode::Right, NONE), (KeyCode::Enter, NONE)],
        key_label: "→ Enter",
        short: "open",
        long: "Expand a server, or open a channel or DM to read its messages",
        footer: Some(1),
    },
    Binding {
        action: Action::Back,
        ctx: Ctx::Where,
        keys: &[(KeyCode::Left, NONE)],
        key_label: "←",
        short: "collapse",
        long: "Collapse a server, or go up from a channel to its server",
        footer: None,
    },
    Binding {
        action: Action::Mark,
        ctx: Ctx::Messages,
        keys: &[key(' ')],
        key_label: "Space",
        short: "mark",
        long: "Mark or unmark one of your messages. Only marked messages are deleted",
        footer: Some(0),
    },
    Binding {
        action: Action::Older,
        ctx: Ctx::Messages,
        keys: &[key('o')],
        key_label: "o",
        short: "older",
        long: "Delete your messages older than this one. This one stays",
        footer: Some(1),
    },
    Binding {
        action: Action::Newer,
        ctx: Ctx::Messages,
        keys: &[key('n')],
        key_label: "n",
        short: "newer",
        long: "Delete your messages newer than this one. This one stays",
        footer: Some(2),
    },
    Binding {
        action: Action::ClearRange,
        ctx: Ctx::Messages,
        keys: &[key('c')],
        key_label: "c",
        short: "clear",
        long: "Clear the range, so the plan deletes all your messages in the scope",
        footer: Some(7),
    },
    Binding {
        action: Action::Open,
        ctx: Ctx::Messages,
        keys: &[(KeyCode::Enter, NONE)],
        key_label: "Enter",
        short: "retry",
        long: "Load the messages again after a failed load",
        footer: None,
    },
    Binding {
        action: Action::Back,
        ctx: Ctx::Messages,
        keys: &[(KeyCode::Left, NONE), (KeyCode::Esc, NONE)],
        key_label: "← Esc",
        short: "back",
        long: "Go back to Where",
        footer: Some(8),
    },
    Binding {
        action: Action::Move,
        ctx: Ctx::Workspace,
        keys: &[
            (KeyCode::Up, NONE),
            (KeyCode::Down, NONE),
            key('k'),
            key('j'),
            (KeyCode::PageUp, NONE),
            (KeyCode::PageDown, NONE),
            (KeyCode::Home, NONE),
            (KeyCode::End, NONE),
        ],
        key_label: "↑↓ j k PgUp PgDn",
        short: "move",
        long: "Move. Below the last message, load older messages",
        footer: None,
    },
    Binding {
        action: Action::SwitchPane,
        ctx: Ctx::Workspace,
        keys: &[(KeyCode::Tab, NONE), (KeyCode::BackTab, NONE)],
        key_label: "Tab",
        short: "pane",
        long: "Go between Where and Messages",
        footer: None,
    },
    Binding {
        action: Action::Find,
        ctx: Ctx::Workspace,
        keys: &[key('/')],
        key_label: "/",
        short: "find",
        long: "Find servers, channels, and DMs by name",
        footer: Some(10),
    },
    Binding {
        action: Action::Preview,
        ctx: Ctx::Workspace,
        keys: &[key('p')],
        key_label: "p",
        short: "preview",
        long: "Count the exact messages that the plan deletes. Nothing is deleted",
        footer: Some(5),
    },
    Binding {
        action: Action::Delete,
        ctx: Ctx::Workspace,
        keys: &[key('d')],
        key_label: "d",
        short: "delete",
        long: "Delete the previewed messages, after you confirm with y",
        footer: Some(6),
    },
    Binding {
        action: Action::Filters,
        ctx: Ctx::Workspace,
        keys: &[key('f')],
        key_label: "f",
        short: "filters",
        long: "Filters and speed: text, type, dates, pinned, workers, delay",
        footer: Some(11),
    },
    Binding {
        action: Action::Link,
        ctx: Ctx::Workspace,
        keys: &[key('l')],
        key_label: "l",
        short: "link",
        long: "Paste a Discord link or a DM channel ID to add it to the scope",
        footer: None,
    },
    Binding {
        action: Action::Source,
        ctx: Ctx::Workspace,
        keys: &[key('s')],
        key_label: "s",
        short: "source",
        long: "Source: account token or data package",
        footer: None,
    },
    Binding {
        action: Action::Reload,
        ctx: Ctx::Workspace,
        keys: &[key('r')],
        key_label: "r",
        short: "reload",
        long: "Reload the servers and DMs",
        footer: None,
    },
    Binding {
        action: Action::Palette,
        ctx: Ctx::Workspace,
        keys: &[key(':'), (KeyCode::Char('k'), CTRL)],
        key_label: ": Ctrl-K",
        short: "commands",
        long: "Open the command palette",
        footer: Some(12),
    },
    Binding {
        action: Action::Help,
        ctx: Ctx::Workspace,
        keys: &[key('?')],
        key_label: "?",
        short: "keys",
        long: "Show all keys",
        footer: Some(13),
    },
    Binding {
        action: Action::Quit,
        ctx: Ctx::Workspace,
        keys: &[key('q')],
        key_label: "q",
        short: "quit",
        long: "Quit",
        footer: Some(14),
    },
    Binding {
        action: Action::PauseResume,
        ctx: Ctx::Run,
        keys: &[key(' '), key('p')],
        key_label: "Space",
        short: "pause/continue",
        long: "Pause or continue the run",
        footer: Some(0),
    },
    Binding {
        action: Action::Stop,
        ctx: Ctx::Run,
        keys: &[key('s'), key('q'), (KeyCode::Esc, NONE)],
        key_label: "s",
        short: "stop",
        long: "Stop the run after the current deletes, and write the report",
        footer: Some(1),
    },
    Binding {
        action: Action::NewSelection,
        ctx: Ctx::Done,
        keys: &[(KeyCode::Enter, NONE)],
        key_label: "Enter",
        short: "new selection",
        long: "Go back to the workspace with fresh message history",
        footer: Some(0),
    },
    Binding {
        action: Action::Quit,
        ctx: Ctx::Done,
        keys: &[key('q'), (KeyCode::Esc, NONE)],
        key_label: "q",
        short: "quit",
        long: "Quit",
        footer: Some(1),
    },
];

/// Whether `event` presses one of `keys`. A plain character also matches with
/// Shift, because terminals send `?` and `:` with Shift held.
fn matches(keys: &[(KeyCode, KeyModifiers)], event: KeyEvent) -> bool {
    let mods = event.modifiers - KeyModifiers::SHIFT;
    keys.iter()
        .any(|&(code, wanted)| code == event.code && mods == wanted)
}

/// The action that `event` starts in `ctxs`, earlier contexts first.
pub(crate) fn lookup(ctxs: &[Ctx], event: KeyEvent) -> Option<Action> {
    ctxs.iter().find_map(|&ctx| {
        BINDINGS
            .iter()
            .find(|b| b.ctx == ctx && matches(b.keys, event))
            .map(|b| b.action)
    })
}

/// The footer bindings for `ctxs`, in priority order.
pub(crate) fn footer(ctxs: &[Ctx]) -> Vec<&'static Binding> {
    let mut shown: Vec<&Binding> = BINDINGS
        .iter()
        .filter(|b| ctxs.contains(&b.ctx) && b.footer.is_some())
        .collect();
    shown.sort_by_key(|b| b.footer);
    shown
}

/// The first key of `binding`, as an event, so the palette can run it.
pub(crate) fn first_key(binding: &Binding) -> KeyEvent {
    let (code, mods) = binding.keys[0];
    KeyEvent::new(code, mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_choose_the_action() {
        let ctxs = [Ctx::Where, Ctx::Workspace];
        let press = |code, mods| lookup(&ctxs, KeyEvent::new(code, mods));
        assert_eq!(press(KeyCode::Char('d'), NONE), Some(Action::Delete));
        assert_eq!(
            press(KeyCode::Char('d'), CTRL),
            None,
            "Ctrl-D must not delete"
        );
        assert_eq!(press(KeyCode::Char('k'), CTRL), Some(Action::Palette));
        assert_eq!(press(KeyCode::Char('k'), NONE), Some(Action::Move));
        assert_eq!(
            press(KeyCode::Char('?'), KeyModifiers::SHIFT),
            Some(Action::Help),
            "terminals send ? with Shift"
        );
    }

    #[test]
    fn no_key_starts_two_actions_in_one_place() {
        for ctxs in [
            [Ctx::Where, Ctx::Workspace],
            [Ctx::Messages, Ctx::Workspace],
        ] {
            let mut seen = Vec::new();
            for binding in BINDINGS.iter().filter(|b| ctxs.contains(&b.ctx)) {
                for key in binding.keys {
                    assert!(!seen.contains(key), "{key:?} is bound twice in {ctxs:?}");
                    seen.push(*key);
                }
            }
        }
    }
}
