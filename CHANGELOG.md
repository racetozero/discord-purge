# Changelog

## Unreleased

## 0.0.1 - 2026-11-05

The first public release.

### Added

- Delete your own Discord messages from a terminal UI, a local web panel
  (`--web`), or a plain command for scripts.
- Preview every run first. Nothing is deleted until you review the exact scope
  and confirm it. A plain command deletes only with `--execute`.
- Read your Discord data package as a ZIP file or a folder, or search one server
  or DM live. Package previews need no token and no network access.
- Select any mix of servers, channels, and DMs. Delete all messages, messages
  older or newer than one message, or single marked messages.
- Filter by text, regular expression, type, and date. Pinned messages are kept
  by default.
- Save your token once with `discord-purge login` and use it in all three
  interfaces.
- Pause, stop, and continue a run. Completed messages are saved for each
  account, so the next run starts where the last one stopped.
- Install on macOS, Linux, and Windows with one command, or with Homebrew. The
  installers verify the SHA-256 checksum of each release.
- Update to the latest release with `discord-purge update`. It verifies the
  SHA-256 checksum of the release before it replaces the executable.
- Release binaries for x64 and ARM64 on macOS, Linux (glibc and musl), and
  Windows. All binaries use profile-guided optimization except on Windows ARM64.
