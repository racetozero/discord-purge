//! Message classification and filtering, done entirely offline from data the
//! package already carries (no API request). A message is classified once into
//! a `Kind` bitmask (text / image / video / audio / voice / file / link), and
//! the user's filters (content substring or regex, date range, type) are
//! applied against that plus the snowflake-encoded timestamp.

use chrono::{DateTime, Utc};

/// The Discord epoch (2015-01-01T00:00:00Z) in milliseconds since the Unix
/// epoch. A snowflake's top 42 bits are the millisecond offset from here.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
pub const TYPE_NAMES: [&str; 7] = ["text", "image", "video", "audio", "voice", "file", "link"];

pub fn snowflake_time(id: u64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(((id >> 22) + DISCORD_EPOCH_MS) as i64)
}

/// A bitmask of what a message contains. Hand-written (no external bitflags
/// crate) because the set is tiny and fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Kind(pub u8);

impl Kind {
    pub const TEXT: Kind = Kind(1 << 0);
    pub const IMAGE: Kind = Kind(1 << 1);
    pub const VIDEO: Kind = Kind(1 << 2);
    pub const AUDIO: Kind = Kind(1 << 3);
    pub const VOICE: Kind = Kind(1 << 4);
    pub const FILE: Kind = Kind(1 << 5);
    pub const LINK: Kind = Kind(1 << 6);
    pub const MEDIA: Kind =
        Kind(Kind::IMAGE.0 | Kind::VIDEO.0 | Kind::AUDIO.0 | Kind::VOICE.0 | Kind::FILE.0);

    pub const fn empty() -> Self {
        Kind(0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Kind {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Kind(self.0 | rhs.0)
    }
}
impl std::ops::BitOrAssign for Kind {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl std::ops::BitAnd for Kind {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Kind(self.0 & rhs.0)
    }
}

pub fn classify<'a>(content: &str, attachments: impl IntoIterator<Item = &'a str>) -> Kind {
    let kinds = attachments
        .into_iter()
        .fold(Kind::empty(), |kinds, url| kinds | attachment_kind(url));
    classify_parts(content, kinds)
}

/// Classify a message whose attachment kinds are already known. Every
/// attachment has a kind, so an empty set means no attachment.
pub(crate) fn classify_parts(content: &str, attachments: Kind) -> Kind {
    let mut kinds = attachments;
    if attachments.is_empty() && !content.trim().is_empty() {
        kinds |= Kind::TEXT;
    }
    if has_link(content) {
        kinds |= Kind::LINK;
    }
    kinds
}

pub(crate) fn attachment_kind(url: &str) -> Kind {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    if base == "voice-message.ogg" {
        return Kind::VOICE;
    }
    let ext = base.rsplit('.').next().unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif" | "bmp" | "tiff" | "svg"
        | "apng" | "avif" => Kind::IMAGE,
        "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v" | "wmv" | "flv" => Kind::VIDEO,
        "mp3" | "ogg" | "oga" | "wav" | "flac" | "m4a" | "aac" | "opus" | "weba" => Kind::AUDIO,
        _ => Kind::FILE,
    }
}

fn has_link(text: &str) -> bool {
    contains_ignore_ascii_case(text, "http://") || contains_ignore_ascii_case(text, "https://")
}

/// Case-insensitive ASCII substring search without a lowercase copy.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    needle.is_empty()
        || haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

pub fn type_mask(id: &str) -> Kind {
    match id {
        "text" => Kind::TEXT,
        "media" => Kind::MEDIA,
        "image" => Kind::IMAGE,
        "video" => Kind::VIDEO,
        "audio" => Kind::AUDIO,
        "voice" => Kind::VOICE,
        "file" => Kind::FILE,
        "link" => Kind::LINK,
        _ => Kind::empty(),
    }
}

#[derive(Debug, Clone, Default)]
pub enum ContentMatch {
    #[default]
    Any,
    Contains(String),
    Regex(regex::Regex),
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub content: ContentMatch,
    /// Combined type mask; empty means "any type".
    pub types: Kind,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub after_id: Option<String>,
    pub before_id: Option<String>,
}

impl Filter {
    pub fn with_inputs(
        mut self,
        content: Option<&str>,
        types: Option<&str>,
        after: Option<&str>,
        before: Option<&str>,
    ) -> anyhow::Result<Self> {
        if let Some(content) = content {
            self.content = Self::parse_content(content)?;
        }
        if let Some(types) = types {
            self.types = Kind::empty();
            for value in types
                .split(',')
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                let mask = type_mask(value);
                anyhow::ensure!(
                    !mask.is_empty(),
                    "You selected an unknown type {value:?}. Use text, media, image, video, audio, voice, file, or link."
                );
                self.types |= mask;
            }
        }
        for (value, target) in [(after, &mut self.after), (before, &mut self.before)] {
            if let Some(value) = value.filter(|text| !text.is_empty()) {
                let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| anyhow::anyhow!(
                    "You entered an invalid date {value:?}. Use YYYY-MM-DD, for example 2026-01-31."))?;
                *target = Some(date.and_time(chrono::NaiveTime::MIN).and_utc());
            }
        }
        anyhow::ensure!(
            self.after.zip(self.before).is_none_or(|(a, b)| a <= b),
            "You set an invalid date range. Set the start date on or before the end date."
        );
        Ok(self)
    }

    /// Parse a content filter string: `/pattern/` or `/pattern/i` is a regex,
    /// anything else is a case-insensitive substring. Empty is "any".
    pub fn parse_content(text: &str) -> anyhow::Result<ContentMatch> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(ContentMatch::Any);
        }
        if let Some(rest) = text.strip_prefix('/')
            && let Some(end) = rest.rfind('/')
        {
            let pattern = &rest[..end];
            let flags = &rest[end + 1..];
            anyhow::ensure!(
                flags.is_empty() || flags == "i",
                "You entered unsupported regex flags {flags:?}. Use /pattern/ or /pattern/i."
            );
            let case_insensitive = flags == "i";
            let expression = if case_insensitive {
                format!("(?i:{pattern})")
            } else {
                pattern.to_string()
            };
            let re = regex::Regex::new(&expression).map_err(|error| {
                anyhow::anyhow!(
                    "the regex {text:?} is invalid: {error}. Use /pattern/ or /pattern/i."
                )
            })?;
            return Ok(ContentMatch::Regex(re));
        }
        Ok(ContentMatch::Contains(text.to_ascii_lowercase()))
    }

    /// A non-numeric ID never passes.
    pub fn keep(&self, id: &str, content: &str, kind: Kind) -> bool {
        id.parse().is_ok_and(|id| self.keep_id(id, content, kind))
    }

    pub fn keep_id(&self, id: u64, content: &str, kind: Kind) -> bool {
        if !self.types.is_empty() && (self.types & kind).is_empty() {
            return false;
        }
        match &self.content {
            ContentMatch::Any => {}
            ContentMatch::Contains(sub) => {
                if !contains_ignore_ascii_case(content, sub) {
                    return false;
                }
            }
            ContentMatch::Regex(regex) => {
                if !regex.is_match(content) {
                    return false;
                }
            }
        }
        if self.after.is_some() || self.before.is_some() {
            let time = snowflake_time(id);
            if self
                .after
                .is_some_and(|a| time.is_some_and(|time| time < a))
                || self
                    .before
                    .is_some_and(|b| time.is_some_and(|time| time > b))
            {
                return false;
            }
        }
        if self.after_id.is_some() || self.before_id.is_some() {
            let id = id.to_string();
            if let Some(a) = &self.after_id
                && cmp_snowflake(&id, a) == std::cmp::Ordering::Less
            {
                return false;
            }
            if let Some(b) = &self.before_id
                && cmp_snowflake(&id, b) == std::cmp::Ordering::Greater
            {
                return false;
            }
        }
        true
    }
}

/// A filter as the text a person types: the inverse of `Filter::with_inputs`.
/// The TUI form and the web page fill their fields from it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct FilterInputs {
    pub content: String,
    pub types: String,
    pub after_date: String,
    pub before_date: String,
}

impl Filter {
    pub fn inputs(&self) -> FilterInputs {
        let content = match &self.content {
            ContentMatch::Any => String::new(),
            ContentMatch::Contains(text) => text.clone(),
            ContentMatch::Regex(regex) => match regex
                .as_str()
                .strip_prefix("(?i:")
                .and_then(|p| p.strip_suffix(')'))
            {
                Some(pattern) => format!("/{pattern}/i"),
                None => format!("/{}/", regex.as_str()),
            },
        };
        let types = TYPE_NAMES
            .into_iter()
            .filter(|name| self.types.contains(type_mask(name)))
            .collect::<Vec<_>>()
            .join(",");
        let date = |d: Option<DateTime<Utc>>| d.map(|d| d.format("%Y-%m-%d").to_string());
        FilterInputs {
            content,
            types,
            after_date: date(self.after).unwrap_or_default(),
            before_date: date(self.before).unwrap_or_default(),
        }
    }
}

/// Compare two snowflake ids numerically (by length then lexicographically,
/// which is correct for equal-length decimal numbers).
pub fn cmp_snowflake(a: &str, b: &str) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_filter_uses_the_snowflake_time() {
        // This snowflake is from 2016-04-30.
        let id = 175928847299117063;
        let after = |date| {
            Filter::default()
                .with_inputs(None, None, Some(date), None)
                .unwrap()
        };
        assert!(after("2016-04-01").keep_id(id, "", Kind::TEXT));
        assert!(!after("2016-05-01").keep_id(id, "", Kind::TEXT));
    }

    #[test]
    fn classify_detects_image_and_link() {
        let kinds = classify("see https://x.com", ["https://cdn/x.png"]);
        assert!(kinds.contains(Kind::IMAGE));
        assert!(kinds.contains(Kind::LINK));
        assert!(!kinds.contains(Kind::TEXT)); // has an attachment, so not pure text
    }

    #[test]
    fn regex_content_filter() {
        let c = Filter::parse_content("/^hi/i").unwrap();
        let f = Filter {
            content: c,
            ..Default::default()
        };
        assert!(f.keep("1", "HImom", Kind::TEXT));
        assert!(!f.keep("1", "bye", Kind::TEXT));
    }

    #[test]
    fn inputs_round_trip_through_with_inputs() {
        let typed = FilterInputs {
            content: "/^hi/i".into(),
            types: "image,link".into(),
            after_date: "2025-01-01".into(),
            before_date: "2025-12-31".into(),
        };
        let filter = Filter::default()
            .with_inputs(
                Some(&typed.content),
                Some(&typed.types),
                Some(&typed.after_date),
                Some(&typed.before_date),
            )
            .unwrap();
        assert_eq!(filter.inputs(), typed);
        assert_eq!(Filter::default().inputs(), FilterInputs::default());
    }

    #[test]
    fn type_filter_excludes() {
        let f = Filter {
            types: Kind::IMAGE,
            ..Default::default()
        };
        assert!(f.keep("1", "", Kind::IMAGE));
        assert!(!f.keep("1", "hello", Kind::TEXT));
    }
}
