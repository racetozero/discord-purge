//! `discord-purge update`: replace this binary with the latest GitHub release.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

const REPOSITORY: &str = "racetozero/discord-purge";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTALLER: &str = if cfg!(windows) {
    "install.ps1"
} else {
    "install.sh"
};

pub async fn run() -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg!(debug_assertions),
        "You ran `discord-purge update` from a debug build, which it must not replace. Install a release, then run it again."
    );
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("Cannot find this executable. Install the release by hand with the command in the README.")?;
    anyhow::ensure!(
        !installed_by_homebrew(&executable),
        "You ran `discord-purge update` on a copy that Homebrew installed, and Homebrew must replace it. Run `brew upgrade racetozero/tap/discord-purge`."
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("discord-purge/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Cannot prepare the update request. Retry, or install the release again.")?;
    let release: serde_json::Value = client
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .context(
            "Cannot reach GitHub to find the latest release. Check your connection and retry.",
        )?
        .error_for_status()
        .context("GitHub did not return the latest release. Wait a few minutes and retry.")?
        .json()
        .await
        .context("GitHub returned invalid release data. Wait a few minutes and retry.")?;
    let tag = release["tag_name"]
        .as_str()
        .context("The latest GitHub release has no tag. Wait a few minutes and retry.")?;
    let latest = version_parts(tag).with_context(|| {
        format!("The latest GitHub release has the tag {tag:?}, but a tag must look like v1.2.3. Install a release by hand.")
    })?;
    let current = version_parts(CURRENT_VERSION).with_context(|| {
        format!("This build has the version {CURRENT_VERSION:?}, but a version must look like 1.2.3. Install a release by hand.")
    })?;
    if latest <= current {
        println!("discord-purge v{CURRENT_VERSION} is up to date.");
        return Ok(());
    }
    let version = tag.trim_start_matches('v');
    println!("Updating discord-purge from v{CURRENT_VERSION} to v{version}...");

    let url = format!("https://raw.githubusercontent.com/{REPOSITORY}/v{version}/{INSTALLER}");
    let script = client
        .get(&url)
        .send()
        .await
        .with_context(|| {
            format!("Cannot download the installer from {url}. Check your connection and retry.")
        })?
        .error_for_status()
        .with_context(|| {
            format!("The installer is not available at {url}. Wait a few minutes and retry.")
        })?
        .bytes()
        .await
        .with_context(|| {
            format!("Cannot read the installer from {url}. Check your connection and retry.")
        })?;
    let staging = tempfile::tempdir().context(
        "Cannot create a temporary folder for the update. Check your disk space and retry.",
    )?;
    let installer = staging.path().join(INSTALLER);
    std::fs::write(&installer, script)
        .context("Cannot save the installer. Check your disk space and retry.")?;

    let binary = run_installer(&installer, version, staging.path()).await?;
    self_replace::self_replace(&binary).context(
        "Cannot replace the discord-purge executable. Check that you can write to its folder, then retry.",
    )?;
    println!("Updated discord-purge to v{version}.");
    Ok(())
}

/// Homebrew keeps a formula at `Cellar/<formula>/<version>/bin/<binary>`.
fn installed_by_homebrew(executable: &Path) -> bool {
    executable
        .ancestors()
        .nth(3)
        .is_some_and(|formula| formula.ends_with("Cellar/discord-purge"))
}

/// The numeric parts of `1.2.3` or `v1.2.3`. Tuples compare 0.0.10 above 0.0.9.
fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.strip_prefix('v').unwrap_or(version).split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn installer_command(
    installer: &Path,
    version: &str,
    staging: &Path,
) -> anyhow::Result<tokio::process::Command> {
    let mut command = if cfg!(windows) {
        // A full path, so a program named powershell in the current folder or PATH cannot run.
        let system_root = std::env::var_os("SystemRoot").context(
            "SystemRoot is not set, so PowerShell cannot be found. Set it to your Windows folder and retry.",
        )?;
        let mut command = tokio::process::Command::new(
            PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        );
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        command
    } else {
        tokio::process::Command::new("/bin/sh")
    };
    command
        .arg(installer)
        .current_dir(staging)
        .env("DISCORD_PURGE_VERSION", version)
        .env("DISCORD_PURGE_INSTALL_DIR", staging)
        .env("DISCORD_PURGE_REPOSITORY", REPOSITORY)
        // A set URL also stops the installer from using the gh login of this account.
        .env(
            "DISCORD_PURGE_RELEASES_URL",
            format!("https://github.com/{REPOSITORY}/releases"),
        )
        .env_remove("DISCORD_PURGE_TARGET")
        // The installer needs no account access.
        .env_remove("DISCORD_TOKEN");
    Ok(command)
}

async fn run_installer(installer: &Path, version: &str, staging: &Path) -> anyhow::Result<PathBuf> {
    let output = installer_command(installer, version, staging)?
        .output()
        .await
        .context("Cannot start the installer. Install the release by hand with the command in the README.")?;
    if !output.status.success() {
        anyhow::bail!(
            "The installer failed ({}): {}\nRetry, or install the release by hand with the command in the README.",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let binary = staging.join(format!("discord-purge{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        binary.is_file(),
        "The installer did not create {}. Retry, or install the release by hand with the command in the README.",
        binary.display()
    );
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_and_reject_other_tags() {
        assert!(version_parts("v0.0.10") > version_parts("0.0.9"));
        assert!(version_parts("v1.0.0") > version_parts("0.9.9"));
        assert_eq!(version_parts("v0.0.1"), version_parts("0.0.1"));
        for invalid in ["release-1", "1.2", "1.2.3.4", "1.2.3-beta", ""] {
            assert_eq!(version_parts(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn only_a_homebrew_cellar_path_counts_as_homebrew() {
        for (path, expected) in [
            (
                "/opt/homebrew/Cellar/discord-purge/0.0.1/bin/discord-purge",
                true,
            ),
            ("/usr/local/bin/discord-purge", false),
            ("/opt/homebrew/Cellar/other/0.0.1/bin/discord-purge", false),
            ("discord-purge", false),
        ] {
            assert_eq!(installed_by_homebrew(Path::new(path)), expected, "{path}");
        }
    }

    #[test]
    fn the_installer_does_not_receive_the_account_token() {
        let staging = Path::new("staging");
        let command = installer_command(&staging.join(INSTALLER), "1.2.3", staging).unwrap();
        let removed = command
            .as_std()
            .get_envs()
            .any(|(name, value)| name == "DISCORD_TOKEN" && value.is_none());
        assert!(removed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_installer_stages_the_requested_version_and_reports_failure() {
        let staging = tempfile::tempdir().unwrap();
        let installer = staging.path().join("install.sh");
        std::fs::write(
            &installer,
            "printf '%s' \"$DISCORD_PURGE_VERSION\" > \"$DISCORD_PURGE_INSTALL_DIR/discord-purge\"\n",
        )
        .unwrap();
        let binary = run_installer(&installer, "1.2.3", staging.path())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(binary).unwrap(), "1.2.3");

        std::fs::write(&installer, "echo checksum mismatch >&2\nexit 1\n").unwrap();
        let error = run_installer(&installer, "1.2.3", staging.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
    }
}
