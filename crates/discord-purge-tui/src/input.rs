//! One text input with a cursor, for every field: token, path, link, query,
//! and filters.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TextInput {
    pub text: String,
    /// The cursor, in characters from the start.
    pub cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            cursor: text.chars().count(),
            text,
        }
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Edit with `event`. Returns false when the key is not an edit, so the
    /// caller can use it.
    pub fn handle(&mut self, event: KeyEvent) -> bool {
        // Windows reports AltGr as Ctrl+Alt, and AltGr types `@`, `\`, and `{`
        // on many layouts. So both together count as neither.
        let (ctrl, alt) = (
            event.modifiers.contains(KeyModifiers::CONTROL),
            event.modifiers.contains(KeyModifiers::ALT),
        );
        let (ctrl, alt) = (ctrl && !alt, alt && !ctrl);
        let len = self.text.chars().count();
        match event.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.truncate(at);
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                self.insert(c.encode_utf8(&mut [0; 4]))
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let (start, end) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(start..end, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let (start, end) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(start..end, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let (from, to) = (self.byte(start), self.byte(self.cursor));
        self.text.replace_range(from..to, "");
        self.cursor = start;
    }
}

/// A path as a file manager pastes it: quoted, with backslash escapes such as
/// `\ ` and `\(`, or as a `file://` URL. Returns the plain path. Windows
/// paths use backslashes as separators, so they keep them.
pub(crate) fn clean_path(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("file://").unwrap_or(text);
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| text.strip_prefix(*q).and_then(|t| t.strip_suffix(*q)));
    match unquoted {
        Some(inner) => inner.to_string(),
        None if cfg!(windows) => text.to_string(),
        None => {
            let mut plain = String::with_capacity(text.len());
            let mut chars = text.chars();
            while let Some(c) = chars.next() {
                plain.extend(if c == '\\' { chars.next() } else { Some(c) });
            }
            plain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(input: &mut TextInput, code: KeyCode, mods: KeyModifiers) {
        assert!(input.handle(KeyEvent::new(code, mods)));
    }

    #[test]
    fn edits_at_the_cursor_and_by_word() {
        let mut input = TextInput::new("héllo world");
        press(&mut input, KeyCode::Left, KeyModifiers::NONE);
        press(&mut input, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(input.text, "héllo worlXd");
        press(&mut input, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(input.text, "héllo d");
        press(&mut input, KeyCode::Home, KeyModifiers::NONE);
        press(&mut input, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input.text, "éllo d");
        press(&mut input, KeyCode::End, KeyModifiers::NONE);
        press(&mut input, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(input.is_empty());
        assert!(!input.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        // AltGr arrives as Ctrl+Alt on Windows and must still type.
        press(
            &mut input,
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(input.text, "@");
    }

    #[test]
    fn dropped_paths_lose_quotes_and_escapes() {
        assert_eq!(clean_path("'/tmp/My package.zip' "), "/tmp/My package.zip");
        #[cfg(not(windows))]
        {
            assert_eq!(clean_path("/tmp/My\\ package.zip"), "/tmp/My package.zip");
            assert_eq!(
                clean_path("/tmp/package\\ \\(1\\).zip"),
                "/tmp/package (1).zip"
            );
        }
        assert_eq!(clean_path("file:///tmp/package.zip"), "/tmp/package.zip");
    }
}
