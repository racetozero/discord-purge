//! Discord data-package parser.
//!
//! This is the most reliable deletion source (the insight from `discord-delete`):
//! the package holds the exact channel and message ids for everything you
//! posted, so deletion needs no search and cannot break when Discord changes
//! its UI. We read a `.zip` or an already-extracted folder, in both the modern
//! JSON layout and the older CSV layout, and both TitleCase and lowercase JSON
//! keys across package versions.
//!
//! Layout (inside the package):
//!
//! ```text
//! messages/
//!   index.json                (channel id -> label, optional)
//!   c<channelid>/ or <id>/
//!     channel.json            { id, type, guild? }
//!     messages.json           [ { ID/Contents/Attachments, ... }, ... ]
//!   ...
//! ```
//!
//! We never read anything else in the package. The `Activity/` telemetry is not
//! touched here (reactions are a separate, optional feature).

use anyhow::Context;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::filter::{Filter, Kind, attachment_kind, classify, classify_parts};
use crate::selection::parse_id;

const MAX_FILE: u64 = 256 * 1024 * 1024;
const TOO_LARGE: &str = "A package message file is larger than 256 MiB. Split the extracted message files before you retry.";

#[derive(Debug, Clone)]
pub struct PkgMessage {
    pub id: String,
    pub content: String,
    pub kind: Kind,
}

/// One channel loaded from the package. `M` is a full message for browsing,
/// or only its numeric ID for a preview.
#[derive(Debug, Clone)]
pub struct RawChannel<M = PkgMessage> {
    pub id: String,
    pub name: String,
    pub guild_id: String, // "" for DMs
    pub guild_name: String,
    pub is_dm: bool,
    pub messages: Vec<M>,
}

impl<M> RawChannel<M> {
    pub fn label(&self) -> String {
        match (self.name.is_empty(), self.guild_name.is_empty()) {
            (false, false) => format!("#{} ({})", self.name, self.guild_name),
            (false, true) => format!("#{}", self.name),
            (true, false) => format!("{} / {}", self.guild_name, self.id),
            (true, true) => format!("DM {}", self.id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedPackage<M = PkgMessage> {
    pub channels: Vec<RawChannel<M>>,
}

impl<M> LoadedPackage<M> {
    pub fn total_messages(&self) -> usize {
        self.channels.iter().map(|c| c.messages.len()).sum()
    }
}

#[derive(Debug, Deserialize)]
struct ChannelJson {
    #[serde(default, alias = "ID")]
    id: Option<String>,
    #[serde(default, alias = "Name")]
    name: Option<String>,
    #[serde(default, alias = "Type", rename = "type")]
    kind: Option<serde_json::Value>,
    #[serde(default, alias = "Guild")]
    guild: Option<serde_json::Value>,
}

pub fn load(path: &Path, filter: &Filter) -> anyhow::Result<LoadedPackage> {
    scan(path, filter, |id, content, kind| PkgMessage {
        id: id.to_string(),
        content: content.to_owned(),
        kind,
    })
}

/// Like `load`, but keep only the IDs of matching messages. Previews and runs
/// need no message text, so this uses much less memory.
pub fn load_ids(path: &Path, filter: &Filter) -> anyhow::Result<LoadedPackage<u64>> {
    scan(path, filter, |id, _, _| id)
}

/// Load one channel with message text. Only that channel's message file is
/// parsed, so browsing a page of history does not read the whole package.
pub fn load_channel(path: &Path, channel_id: &str) -> anyhow::Result<LoadedPackage> {
    scan_where(
        path,
        &Filter::default(),
        |id| id == channel_id,
        |id, content, kind| PkgMessage {
            id: id.to_string(),
            content: content.to_owned(),
            kind,
        },
    )
}

pub fn scan<M>(
    path: &Path,
    filter: &Filter,
    keep: impl FnMut(u64, &str, Kind) -> M,
) -> anyhow::Result<LoadedPackage<M>> {
    scan_where(path, filter, |_| true, keep)
}

fn scan_where<M>(
    path: &Path,
    filter: &Filter,
    wanted: impl Fn(&str) -> bool,
    mut keep: impl FnMut(u64, &str, Kind) -> M,
) -> anyhow::Result<LoadedPackage<M>> {
    let mut files = PackageFiles::open(path)?;
    let index: BTreeMap<String, String> = match files.index.take() {
        Some(entry) => serde_json::from_slice(&files.read(&entry)?).unwrap_or_default(),
        None => BTreeMap::new(),
    };
    anyhow::ensure!(
        files
            .folders
            .values()
            .any(|f| f.messages_json.is_some() || f.messages_csv.is_some()),
        "The package has no message files. Use the Discord export ZIP, its extracted root folder, or its messages folder."
    );

    // Read one channel's files at a time, so only one message file is in
    // memory. Duplicate IDs keep their first occurrence across the package.
    let mut seen = HashSet::new();
    let mut channels = Vec::new();
    for (directory, folder) in std::mem::take(&mut files.folders) {
        let (mut channel_id, mut channel_name, mut guild_id, mut guild_name) =
            (String::new(), String::new(), String::new(), String::new());
        let mut is_dm = false;

        if let Some(entry) = &folder.channel
            && let Ok(parsed) = serde_json::from_slice::<ChannelJson>(&files.read(entry)?)
        {
            channel_id = parsed.id.unwrap_or_default();
            channel_name = parsed.name.unwrap_or_default();
            is_dm = is_dm_type(parsed.kind.as_ref());
            (guild_id, guild_name) = guild_of(parsed.guild.as_ref());
        }
        if channel_id.is_empty() {
            // Folders are named `c<channel id>` or `<channel id>`.
            let name = directory.rsplit('/').next().unwrap_or(&directory);
            channel_id = name.chars().filter(char::is_ascii_digit).collect();
        }
        anyhow::ensure!(
            parse_id(&channel_id).is_some(),
            "The package has an invalid channel ID {channel_id:?}. Use an unmodified Discord data package."
        );
        // A channel with no guild object is treated as a DM.
        if guild_id.is_empty()
            && !is_dm
            && let Some(label) = index.get(&channel_id)
            && let Some((n, g)) = split_label(label)
        {
            if channel_name.is_empty() {
                channel_name = n;
            }
            guild_name = g;
        }
        is_dm = guild_id.is_empty() && guild_name.is_empty();
        if !wanted(&channel_id) {
            continue;
        }

        let kept = if let Some(entry) = &folder.messages_json {
            parse_messages_json(files.open_file(entry)?.1, filter, &mut seen, &mut keep)?
        } else if let Some(entry) = &folder.messages_csv {
            parse_messages_csv(files.open_file(entry)?.1, filter, &mut seen, &mut keep)?
        } else {
            continue;
        };
        if kept.is_empty() {
            continue;
        }
        channels.push(RawChannel {
            id: channel_id,
            name: channel_name,
            guild_id,
            guild_name,
            is_dm,
            messages: kept,
        });
    }

    channels.sort_by_cached_key(RawChannel::label);
    Ok(LoadedPackage { channels })
}

enum Entry {
    Path(PathBuf),
    Zip(usize),
}

#[derive(Default)]
struct FolderFiles {
    channel: Option<Entry>,
    messages_json: Option<Entry>,
    messages_csv: Option<Entry>,
}

struct PackageFiles {
    zip: Option<zip::ZipArchive<std::fs::File>>,
    index: Option<Entry>,
    folders: BTreeMap<String, FolderFiles>,
}

impl PackageFiles {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let mut files = Self {
            zip: None,
            index: None,
            folders: BTreeMap::new(),
        };
        if path.is_dir() {
            let mut found = Vec::new();
            walk_dir(path, path, &mut found)?;
            for (name, path) in found {
                files.add(&name, Entry::Path(path));
            }
            return Ok(files);
        }
        let file = std::fs::File::open(path)
            .map_err(|error| anyhow::anyhow!("could not open the package {:?}: {error}", path))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(
                "the package {:?} is not a readable .zip or folder: {error}",
                path
            )
        })?;
        for i in 0..zip.len() {
            let f = zip.by_index_raw(i)?;
            let name = f.name().replace('\\', "/");
            if f.is_file() && message_file(&name) {
                drop(f);
                files.add(&name, Entry::Zip(i));
            }
        }
        files.zip = Some(zip);
        Ok(files)
    }

    fn add(&mut self, name: &str, entry: Entry) {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with("messages/index.json") {
            self.index.get_or_insert(entry);
            return;
        }
        let folder = self
            .folders
            .entry(parent_dir(name).to_string())
            .or_default();
        match lower.rsplit('/').next() {
            Some("channel.json") => folder.channel = Some(entry),
            Some("messages.json") => folder.messages_json = Some(entry),
            Some("messages.csv") => folder.messages_csv = Some(entry),
            _ => {}
        }
    }

    fn open_file(&mut self, entry: &Entry) -> anyhow::Result<(u64, Box<dyn Read + '_>)> {
        let (size, reader): (u64, Box<dyn Read + '_>) = match entry {
            Entry::Path(path) => {
                let file = std::fs::File::open(path)?;
                (file.metadata()?.len(), Box::new(file))
            }
            Entry::Zip(i) => {
                let file = self
                    .zip
                    .as_mut()
                    .context("A ZIP entry has no open ZIP file. Open the package again and retry.")?
                    .by_index(*i)?;
                (file.size(), Box::new(file))
            }
        };
        anyhow::ensure!(size <= MAX_FILE, TOO_LARGE);
        Ok((size, Box::new(reader.take(MAX_FILE + 1))))
    }

    fn read(&mut self, entry: &Entry) -> anyhow::Result<Vec<u8>> {
        let (size, mut reader) = self.open_file(entry)?;
        // Reserve the exact size so a large file is not reallocated while read.
        let mut bytes = Vec::with_capacity(size as usize + 1);
        reader.read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= MAX_FILE, TOO_LARGE);
        Ok(bytes)
    }
}

fn message_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.split('/').any(|part| part == "messages")
        && matches!(
            name.rsplit('/').next(),
            Some("index.json" | "channel.json" | "messages.json" | "messages.csv")
        )
}

fn walk_dir(root: &Path, directory: &Path, out: &mut Vec<(String, PathBuf)>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk_dir(root, &path, out)?;
        } else if kind.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let relative_path = if root
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("messages"))
            {
                format!("messages/{relative_path}")
            } else {
                relative_path
            };
            if message_file(&relative_path) {
                out.push((relative_path, path));
            }
        }
    }
    Ok(())
}

fn split_label(label: &str) -> Option<(String, String)> {
    let (channel, guild) = label.rsplit_once(" in ")?;
    Some((channel.trim().to_string(), guild.trim().to_string()))
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn is_dm_type(kind: Option<&serde_json::Value>) -> bool {
    match kind {
        Some(serde_json::Value::String(s)) => {
            let s = s.to_ascii_uppercase();
            s == "DM" || s == "GROUP_DM"
        }
        // Numeric channel types: 1 = DM, 3 = GROUP_DM.
        Some(serde_json::Value::Number(n)) => {
            matches!(n.as_i64(), Some(1) | Some(3))
        }
        _ => false,
    }
}

fn guild_of(guild: Option<&serde_json::Value>) -> (String, String) {
    match guild {
        Some(serde_json::Value::Object(o)) => {
            let id = o
                .get("id")
                .or_else(|| o.get("ID"))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let name = o
                .get("name")
                .or_else(|| o.get("Name"))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            (id, name)
        }
        Some(serde_json::Value::String(s)) => (s.clone(), String::new()),
        _ => (String::new(), String::new()),
    }
}

fn invalid_id(id: impl std::fmt::Debug) -> String {
    format!("The package has an invalid message ID {id:?}. Use an unmodified Discord data package.")
}

/// One row of `messages.json`. Keys have been TitleCase ("ID", "Contents",
/// "Attachments") and lowercase across versions. Text is borrowed from the
/// file when it has no escapes, and other keys such as "Timestamp" are skipped.
#[derive(Deserialize)]
struct Row<'a> {
    #[serde(alias = "ID", alias = "Id", deserialize_with = "message_id")]
    id: u64,
    #[serde(
        borrow,
        default,
        alias = "Contents",
        alias = "contents",
        alias = "Content",
        deserialize_with = "text"
    )]
    content: Cow<'a, str>,
    #[serde(default, alias = "Attachments", alias = "Attachment")]
    attachments: Attachments,
}

fn message_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    struct Id;
    impl Visitor<'_> for Id {
        type Value = u64;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a numeric message ID")
        }
        fn visit_u64<E: serde::de::Error>(self, id: u64) -> Result<u64, E> {
            (id > 0)
                .then_some(id)
                .ok_or_else(|| E::custom(invalid_id(id)))
        }
        fn visit_i64<E: serde::de::Error>(self, id: i64) -> Result<u64, E> {
            Err(E::custom(invalid_id(id)))
        }
        fn visit_str<E: serde::de::Error>(self, id: &str) -> Result<u64, E> {
            parse_id(id).ok_or_else(|| E::custom(invalid_id(id)))
        }
    }
    deserializer.deserialize_any(Id)
}

fn text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Cow<'de, str>, D::Error> {
    struct Text;
    impl<'de> Visitor<'de> for Text {
        type Value = Cow<'de, str>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("message text")
        }
        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
            Ok(Cow::Borrowed(value))
        }
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(Cow::Owned(value.to_owned()))
        }
        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(Cow::Owned(value))
        }
        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Cow::Owned(value.to_string()))
        }
        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Cow::Owned(value.to_string()))
        }
        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
            Ok(Cow::Owned(value.to_string()))
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Cow::Borrowed(""))
        }
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Cow::Borrowed(""))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(Cow::Borrowed(""))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(Cow::Borrowed(""))
        }
    }
    deserializer.deserialize_any(Text)
}

/// The kinds of a row's attachments: space-separated URLs, an array of URLs,
/// or an array of objects with a `url` key. No URL is copied.
#[derive(Default)]
struct Attachments(Kind);

impl<'de> Deserialize<'de> for Attachments {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct List;
        impl<'de> Visitor<'de> for List {
            type Value = Attachments;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("attachment URLs")
            }
            fn visit_str<E>(self, urls: &str) -> Result<Attachments, E> {
                Ok(Attachments(classify("", urls.split_whitespace())))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Attachments, A::Error> {
                let mut kinds = Kind::empty();
                while let Some(Url(kind)) = seq.next_element()? {
                    kinds |= kind.unwrap_or_default();
                }
                Ok(Attachments(kinds))
            }
            fn visit_unit<E>(self) -> Result<Attachments, E> {
                Ok(Attachments::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Attachments, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Attachments::default())
            }
        }
        deserializer.deserialize_any(List)
    }
}

struct Url(Option<Kind>);

impl<'de> Deserialize<'de> for Url {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UrlVisitor;
        impl<'de> Visitor<'de> for UrlVisitor {
            type Value = Url;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an attachment URL or object")
            }
            fn visit_str<E>(self, url: &str) -> Result<Url, E> {
                Ok(Url(Some(attachment_kind(url))))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Url, A::Error> {
                let mut kind = None;
                while let Some(UrlKey(is_url)) = map.next_key()? {
                    if is_url {
                        kind = map.next_value::<Url>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Url(kind))
            }
            fn visit_unit<E>(self) -> Result<Url, E> {
                Ok(Url(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Url, E> {
                Ok(Url(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Url, E> {
                Ok(Url(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Url, E> {
                Ok(Url(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Url, E> {
                Ok(Url(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Url, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Url(None))
            }
        }
        deserializer.deserialize_any(UrlVisitor)
    }
}

struct UrlKey(bool);

impl<'de> Deserialize<'de> for UrlKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Key;
        impl Visitor<'_> for Key {
            type Value = UrlKey;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object key")
            }
            fn visit_str<E>(self, key: &str) -> Result<UrlKey, E> {
                Ok(UrlKey(key == "url"))
            }
        }
        deserializer.deserialize_identifier(Key)
    }
}

/// Parse the array-of-objects `messages.json`, one row at a time, and keep
/// only the rows that pass the filter. The file is read in 1 MiB parts, so
/// memory use does not grow with the file size.
fn parse_messages_json<M>(
    mut input: impl Read,
    filter: &Filter,
    seen: &mut HashSet<u64>,
    keep: &mut impl FnMut(u64, &str, Kind) -> M,
) -> anyhow::Result<Vec<M>> {
    let invalid = |detail: &dyn std::fmt::Display| {
        anyhow::anyhow!(
            "The message JSON is invalid: {detail}. Use an unmodified Discord data package."
        )
    };
    let (mut buffer, mut position) = (Vec::with_capacity(CHUNK), 0);
    let mut out = Vec::new();
    if next_token(&mut input, &mut buffer, &mut position)? != Some(b'[') {
        return Err(invalid(&"it is not an array of message objects"));
    }
    position += 1;
    if next_token(&mut input, &mut buffer, &mut position)? == Some(b']') {
        position += 1;
    } else {
        loop {
            // Parse one row from the buffer. A row cut at the end of the
            // buffer is parsed again after more of the file is read.
            loop {
                let mut rows =
                    serde_json::Deserializer::from_slice(&buffer[position..]).into_iter::<Row>();
                match rows.next() {
                    Some(Ok(row)) => {
                        let end = rows.byte_offset();
                        let kind = classify_parts(&row.content, row.attachments.0);
                        if seen.insert(row.id) && filter.keep_id(row.id, &row.content, kind) {
                            out.push(keep(row.id, &row.content, kind));
                        }
                        position += end;
                        break;
                    }
                    Some(Err(error)) if error.is_eof() => {
                        if !refill(&mut input, &mut buffer, &mut position)? {
                            return Err(invalid(&error));
                        }
                    }
                    Some(Err(error)) => return Err(invalid(&error)),
                    None => return Err(invalid(&"the message array is incomplete")),
                }
            }
            match next_token(&mut input, &mut buffer, &mut position)? {
                Some(b',') => position += 1,
                Some(b']') => {
                    position += 1;
                    break;
                }
                _ => return Err(invalid(&"messages must be separated by commas")),
            }
        }
    }
    if next_token(&mut input, &mut buffer, &mut position)?.is_some() {
        return Err(invalid(&"there is text after the message array"));
    }
    Ok(out)
}

const CHUNK: usize = 1 << 20;

fn next_token(
    input: &mut impl Read,
    buffer: &mut Vec<u8>,
    position: &mut usize,
) -> anyhow::Result<Option<u8>> {
    loop {
        while let Some(&byte) = buffer.get(*position) {
            if !byte.is_ascii_whitespace() {
                return Ok(Some(byte));
            }
            *position += 1;
        }
        if !refill(input, buffer, position)? {
            return Ok(None);
        }
    }
}

fn refill(
    input: &mut impl Read,
    buffer: &mut Vec<u8>,
    position: &mut usize,
) -> anyhow::Result<bool> {
    buffer.drain(..*position);
    *position = 0;
    anyhow::ensure!(
        buffer.len() <= 16 * CHUNK,
        "A message in the package is larger than 16 MiB. Use an unmodified Discord data package."
    );
    let filled = buffer.len();
    buffer.resize(filled + CHUNK, 0);
    let read = input.read(&mut buffer[filled..])?;
    buffer.truncate(filled + read);
    Ok(read > 0)
}

/// Parse the legacy `messages.csv` (header row then rows). The columns are
/// ID, Timestamp, Contents, Attachments in that order in older exports, but we
/// locate them by header name to be safe.
fn parse_messages_csv<M>(
    data: impl Read,
    filter: &Filter,
    seen: &mut HashSet<u64>,
    keep: &mut impl FnMut(u64, &str, Kind) -> M,
) -> anyhow::Result<Vec<M>> {
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(data);
    let headers = reader
        .headers()
        .map_err(|error| {
            anyhow::anyhow!(
                "The CSV header is invalid: {error}. Use the original Discord CSV message file."
            )
        })?
        .clone();
    let column = |want: &[&str]| -> Option<usize> {
        headers.iter().position(|header| {
            let header = header.to_ascii_lowercase();
            want.iter().any(|w| header == *w)
        })
    };
    let id_col = match column(&["id"]) {
        Some(c) => c,
        None => anyhow::bail!(
            "The CSV message file has no ID column. Use the original Discord CSV message file."
        ),
    };
    let content_col = column(&["contents", "content"]);
    let attachment_column = column(&["attachments", "attachment"]);

    let mut out = Vec::new();
    let mut record = csv::StringRecord::new();
    while reader.read_record(&mut record).map_err(|error| {
        anyhow::anyhow!(
            "The message CSV is invalid: {error}. Use the original Discord CSV message file."
        )
    })? {
        let raw = record.get(id_col).unwrap_or("");
        let id = parse_id(raw).ok_or_else(|| anyhow::anyhow!(invalid_id(raw)))?;
        let content = content_col.and_then(|c| record.get(c)).unwrap_or("");
        let attachments = attachment_column.and_then(|c| record.get(c)).unwrap_or("");
        let kind = classify(content, attachments.split_whitespace());
        if seen.insert(id) && filter.keep_id(id, content, kind) {
            out.push(keep(id, content, kind));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_zip() -> tempfile::NamedTempFile {
        let f = tempfile::Builder::new().suffix(".zip").tempfile().unwrap();
        let mut zip = zip::ZipWriter::new(f.reopen().unwrap());
        let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        zip.start_file("messages/c111/channel.json", options)
            .unwrap();
        zip.write_all(br#"{"id":"111","name":"general","guild":{"ID":"900","Name":"My Server"}}"#)
            .unwrap();
        zip.start_file("messages/c111/messages.json", options)
            .unwrap();
        zip.write_all(
            br#"[{"ID":"175928847299117063","contents":"hello world","Attachments":""},
                {"ID":"175928847299117064","Contents":"look https://x.com","Attachments":""}]"#,
        )
        .unwrap();
        zip.finish().unwrap();
        f
    }

    #[test]
    fn loads_messages_from_zip() {
        let f = write_zip();
        let pkg = load(f.path(), &Filter::default()).unwrap();
        assert_eq!(pkg.channels.len(), 1);
        let c = &pkg.channels[0];
        assert_eq!(c.id, "111");
        assert_eq!(c.guild_name, "My Server");
        assert_eq!(c.guild_id, "900");
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].content, "hello world");
        assert_eq!(c.label(), "#general (My Server)");
    }

    #[test]
    fn content_filter_narrows() {
        let f = write_zip();
        let filter = Filter {
            content: Filter::parse_content("https").unwrap(),
            ..Default::default()
        };
        let pkg = load(f.path(), &filter).unwrap();
        assert_eq!(pkg.total_messages(), 1);
    }
    #[test]
    fn extracted_messages_folder_rejects_malformed_rows_and_removes_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("messages/c111");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("messages.json");
        std::fs::write(&path, b"not JSON").unwrap();
        assert!(
            load(root.path(), &Filter::default())
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        std::fs::write(&path, br#"[{"ID":"175928847299117063","Contents":"first"},{"ID":"175928847299117063","Contents":"duplicate"}]"#).unwrap();
        let package = load(&root.path().join("messages"), &Filter::default()).unwrap();
        assert_eq!(package.total_messages(), 1);
        std::fs::write(&path, br#"[{"ID":"../invalid","Contents":"bad ID"}]"#).unwrap();
        assert!(load(root.path(), &Filter::default()).is_err());
    }
    #[test]
    fn rows_across_read_boundaries_keep_their_exact_text() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("messages/c111");
        std::fs::create_dir_all(&folder).unwrap();
        let text = |i: u64| format!("row {i} \"quoted\" \\ ✓\nnext line {}", "x".repeat(300));
        let rows: Vec<_> = (1..=12_000u64)
            .map(|i| serde_json::json!({"ID": (175928847299117063 + i).to_string(), "Contents": text(i)}))
            .collect();
        let json = serde_json::to_vec(&rows).unwrap();
        assert!(
            json.len() > 3 * CHUNK,
            "rows must cross several read boundaries"
        );
        std::fs::write(folder.join("messages.json"), json).unwrap();
        let package = load(root.path(), &Filter::default()).unwrap();
        let messages = &package.channels[0].messages;
        assert_eq!(messages.len(), 12_000);
        for (i, message) in (1..).zip(messages) {
            assert_eq!(message.id, (175928847299117063 + i).to_string());
            assert_eq!(message.content, text(i));
        }
    }

    #[test]
    fn message_parsers_preserve_aliases_attachments_and_validation() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("messages/c111");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("messages.json");
        std::fs::write(&path, br#"[
            {"ID":175928847299117063,"contents":"first","Attachments":[{"url":"https://cdn.example/photo.png"}]},
            {"id":"175928847299117064","content":"line\ntext","attachments":["https://cdn.example/audio.ogg"]},
            {"Id":"175928847299117063","Content":"duplicate","Attachment":"https://cdn.example/other.png"}
        ]"#).unwrap();
        let package = load(root.path(), &Filter::default()).unwrap();
        assert_eq!(package.total_messages(), 2);
        assert_eq!(package.channels[0].messages[1].content, "line\ntext");
        assert!(package.channels[0].messages[0].kind.contains(Kind::IMAGE));
        assert!(package.channels[0].messages[1].kind.contains(Kind::AUDIO));
        let filter = Filter {
            types: Kind::IMAGE,
            ..Default::default()
        };
        assert_eq!(load(root.path(), &filter).unwrap().total_messages(), 1);
        for invalid in [
            r#"[{"ID":"175928847299117064","Contents":"filtered"},{"ID":"bad"}]"#,
            r#"[{"ID":"175928847299117064","Contents":"filtered"}] trailing"#,
            r#"[{"ID":"175928847299117064","Contents":"filtered"},false]"#,
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(
                load(root.path(), &filter).is_err(),
                "filters must not hide invalid trailing input"
            );
        }
        std::fs::remove_file(&path).unwrap();
        let csv = folder.join("messages.csv");
        std::fs::write(&csv, "ID,Timestamp,Contents,Attachments\n175928847299117063,,first,https://cdn.example/photo.png\n175928847299117064,,filtered,\n175928847299117063,,duplicate,https://cdn.example/other.png\n").unwrap();
        assert_eq!(load(root.path(), &filter).unwrap().total_messages(), 1);
        std::fs::write(
            &csv,
            "ID,Contents,Attachments\n175928847299117064,filtered,\nbad,invalid,\n",
        )
        .unwrap();
        assert!(
            load(root.path(), &filter).is_err(),
            "CSV filters must not hide invalid rows"
        );
    }
}
