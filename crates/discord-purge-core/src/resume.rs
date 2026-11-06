//! Resume log and end-of-run report.
//!
//! Every confirmed-gone message (deleted, already gone, or an undeletable
//! system message) has its resume key appended to a per-account log as it
//! happens. A later run reads the log first and filters those keys out before
//! queuing anything, so a crash or a clean exit loses no progress and never
//! repeats a delete. The log is keyed by account id, so it works across
//! different data packages for the same account. Previews never write it.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::engine::Snapshot;
use anyhow::Context;

pub fn state_dir() -> PathBuf {
    if let Ok(duration) = std::env::var("DISCORD_PURGE_STATE_DIR") {
        return PathBuf::from(duration);
    }
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("discord-purge")
}

pub fn log_path(account_id: &str) -> PathBuf {
    state_dir().join(format!("user-{account_id}.deleted.log"))
}

pub fn token_path() -> PathBuf {
    state_dir().join("token")
}

/// Save the token so later runs need no `--token`. On Unix the file is written
/// with owner-only (0600) permissions, because the token controls the account.
pub fn save_token(token: &str) -> anyhow::Result<PathBuf> {
    write_private_file(&token_path(), token.trim().as_bytes())
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> anyhow::Result<PathBuf> {
    let directory = path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "The private file path has no parent directory. Use an absolute state directory path."
        )
    })?;
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(directory).with_context(|| {
        format!(
            "Cannot create private state directory {}. Check the path and directory permissions.",
            directory.display()
        )
    })?;
    let temporary = directory.join(format!(".private-{:032x}", rand::random::<u128>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        return match std::fs::remove_file(&temporary) {
            Err(cleanup) if cleanup.kind() != std::io::ErrorKind::NotFound => Err(error.context(format!(
                "Cannot remove the temporary file {}: {cleanup}. Remove it yourself, because it can hold private data.",
                temporary.display()
            ))),
            _ => Err(error),
        };
    }
    Ok(path.to_path_buf())
}

pub fn load_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()).ok()?;
    Some(token.trim().to_string()).filter(|t| !t.is_empty())
}

pub fn forget_token() -> anyhow::Result<bool> {
    match std::fs::remove_file(token_path()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Cannot remove the saved token at {}. Check the file permissions, or delete the file yourself.",
                token_path().display()
            )
        }),
    }
}

/// How a front end shares the token with the others through the saved token
/// file, so a token that one of them accepts works in all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStore {
    Off,
    Read,
    ReadWrite,
}

impl TokenStore {
    pub fn load(self) -> Option<String> {
        (self != Self::Off).then(load_token).flatten()
    }

    /// Save a token that Discord accepted. Returns true when the saved token
    /// changed, so the front end can say so.
    pub fn remember(self, token: &str) -> anyhow::Result<bool> {
        let token = token.trim();
        if self != Self::ReadWrite || token.is_empty() || load_token().as_deref() == Some(token) {
            return Ok(false);
        }
        save_token(token).map_err(|error| {
            anyhow::anyhow!(
                "Discord accepted the token, but it was not saved to {}: {error}. Make the folder writable, or start with --no-save-token to keep the token in memory only.",
                token_path().display()
            )
        })?;
        Ok(true)
    }
}

pub fn load_done(account_id: &str) -> HashSet<u64> {
    let path = log_path(account_id);
    let mut set = HashSet::new();
    if let Ok(file) = std::fs::File::open(&path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(id) = line.trim().parse() {
                set.insert(id);
            }
        }
    }
    set
}

/// An append-only writer for confirmed-gone keys. Thread-safe; the engine calls
/// `record` from any worker. Flushes each line so a crash keeps what was done.
pub struct ResumeLog {
    file: Mutex<std::fs::File>,
}

impl ResumeLog {
    pub fn open(account_id: &str) -> anyhow::Result<Self> {
        let directory = state_dir();
        std::fs::create_dir_all(&directory).map_err(|error| {
            anyhow::anyhow!("could not create the state dir {:?}: {error}", directory)
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(account_id))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, key: &str) -> anyhow::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writeln!(file, "{key}")?;
        file.sync_data()?;
        Ok(())
    }
}

pub fn write_report(
    account_label: &str,
    snapshot: &Snapshot,
    explicit: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let directory = state_dir().join("reports");
            std::fs::create_dir_all(&directory)?;
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            directory.join(format!("report-{stamp}.txt"))
        }
    };
    let mut body = String::new();
    body.push_str(&format!("discord-purge report\nAccount: {account_label}\n"));
    body.push_str(&format!(
        "Status: {}\n",
        snapshot.phase.label().to_lowercase()
    ));
    if let Some(reason) = snapshot.phase.reason() {
        body.push_str(&format!("Reason: {reason}\n"));
    }
    body.push_str(&format!(
        "Elapsed: {}\nTotal: {}  Deleted: {}  Skipped: {}  Deferred: {}  Failed: {}\n",
        format_duration(snapshot.elapsed),
        snapshot.total,
        snapshot.deleted,
        snapshot.skipped,
        snapshot.deferred,
        snapshot.failed
    ));
    if !snapshot.status.is_empty() {
        body.push_str(&format!("Last status: {}\n", snapshot.status));
    }
    if let Some(notice) = snapshot.limit_notice() {
        body.push_str(&format!("Rate limits: {notice}\n"));
    }
    if !snapshot.forbidden.is_empty() {
        body.push_str("\nChannel/system responses, grouped by channel (response counts):\n");
        let mut rows = snapshot.forbidden.clone();
        rows.sort_by_key(|row| std::cmp::Reverse(row.count));
        for row in rows {
            let label = if row.label.is_empty() {
                "unknown".into()
            } else {
                row.label.clone()
            };
            body.push_str(&format!(
                "  {:<32} {:>6}  reason: {}\n",
                label, row.count, row.reason
            ));
        }
    }
    if !snapshot.errors.is_empty() {
        body.push_str("\nLast errors:\n");
        for error in snapshot.errors.iter().rev().take(20) {
            body.push_str(&format!("  {error}\n"));
        }
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_store_shares_one_saved_token() {
        if std::env::var_os("DISCORD_PURGE_RESUME_TEST_CHILD").is_some() {
            assert_eq!(TokenStore::ReadWrite.load(), None);
            assert!(!TokenStore::Read.remember("first").unwrap());
            assert!(!TokenStore::Off.remember("first").unwrap());
            assert_eq!(load_token(), None);
            assert!(TokenStore::ReadWrite.remember(" first\n").unwrap());
            assert!(!TokenStore::ReadWrite.remember("first").unwrap());
            assert_eq!(TokenStore::Read.load().as_deref(), Some("first"));
            assert_eq!(TokenStore::Off.load(), None);
            assert!(TokenStore::ReadWrite.remember("second").unwrap());
            assert_eq!(TokenStore::ReadWrite.load().as_deref(), Some("second"));
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resume::tests::token_store_shares_one_saved_token",
            ])
            .env("DISCORD_PURGE_STATE_DIR", temporary.path())
            .env("DISCORD_PURGE_RESUME_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn log_roundtrips() {
        // Process environment belongs to a process, so isolate it in a child
        // rather than mutate it while other tests can run.
        if std::env::var_os("DISCORD_PURGE_RESUME_TEST_CHILD").is_some() {
            let log = ResumeLog::open("acct1").unwrap();
            log.record("111").unwrap();
            log.record("222").unwrap();
            drop(log);
            let done = load_done("acct1");
            assert!(done.contains(&111) && done.contains(&222));
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "resume::tests::log_roundtrips"])
            .env("DISCORD_PURGE_STATE_DIR", temporary.path())
            .env("DISCORD_PURGE_RESUME_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
