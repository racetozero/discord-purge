# Changelog

## Unreleased

## 0.0.2 - 2026-11-06

### Added

- A run now shows when it is paused and when it is stopping, in the terminal UI
  and in the web panel. A web page that you reload during a pause still shows
  the pause.
- A new `deferred` count shows messages that a later run can delete, for example
  after you get access to a channel again. `skipped` now counts only messages
  that no retry can delete.
- A preview now tells you when Discord's search stopped early, because its count
  can then be too low.
- You can retry a channel history that did not load.
- Run screens and reports now tell you when repeated shared or global rate limits
  limited a run to one request at a time.

### Changed

- A plain `--execute` run now exits with an error when messages remain to retry.
- Reports and screens now separate a stop that you asked for from a failure, and
  they give the reason for the failure.
- `--execute` or `--report` without `--no-tui` is now an error. So is `--web` with
  `--no-tui`. Before this change, the app ignored these options.

### Fixed

- The web panel no longer stays on the confirmation when a run ends immediately.
- Enter in the web panel's token field no longer clears the selection during a
  run.
- After Discord rejects a token, the next delete asks for a new token.
- All requests for one token now share one rate-limit state. Before this change,
  each browse action kept its own state.
- A finished preview no longer closes a form that you opened in the terminal UI.
- If the selection changes during the start check, the terminal UI starts no
  run and tells you to preview again.
- In the terminal UI's delete form, a pasted token now always goes to the token
  field.

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
