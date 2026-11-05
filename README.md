# discord-purge

[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](rust-toolchain.toml)
[![Downloads](https://img.shields.io/github/downloads/racetozero/discord-purge/total)](https://github.com/racetozero/discord-purge/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue)](LICENSE)

<img width="1822" height="1180" alt="image" src="https://github.com/user-attachments/assets/c52c9439-7dc4-4edc-83ca-8add8e06b829" />

Delete your own Discord messages, safely and at a pace Discord accepts.

discord-purge is one native app with three interfaces: a terminal UI, a local
web panel, and a plain command for scripts. Every run starts as a preview.
Nothing is deleted until you review the exact scope and confirm it.

> [!CAUTION]
> Automation of a user account is against Discord's Terms of Service, and
> Discord can restrict or ban the account. Several steps are built into the app
> to make a run safer than with similar tools. These steps lower the risk. They
> do not remove it.

> [!WARNING]
> Deleted messages cannot be restored.

## Why discord-purge

- **See before you delete.** Each run starts with a preview of the scope and
  the number of matched messages.
- **Delete only your messages.** The app never deletes another user's message,
  also when your account can moderate the channel.
- **Select exactly what you want.** Pick servers, channels, and DMs. Then filter
  by text, type, or date, or mark single messages.
- **Work offline first.** Preview your Discord data package with no token and
  no network access.
- **Continue after a stop.** The next run starts where the last one stopped.
- **Keep your data local.** The web panel runs only on your computer, and the
  app collects no telemetry.

## Install

macOS and Linux:

```bash
curl -LsSf https://raw.githubusercontent.com/racetozero/discord-purge/main/install.sh | sh
```

Windows, including ARM64:

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/racetozero/discord-purge/main/install.ps1 | iex"
```

The installer selects the correct release, verifies its SHA-256 checksum, and
installs `discord-purge` in `~/.local/bin`. Open a new terminal after
installation.

Update later with:

```bash
discord-purge update
```

Homebrew on macOS and Linux:

```bash
brew install racetozero/tap/discord-purge
```

Homebrew owns this copy, so update it with
`brew upgrade racetozero/tap/discord-purge` instead of `discord-purge update`.

## Start in two commands

Preview your data package. This needs no token, and it deletes nothing:

```bash
discord-purge --package package.zip --no-tui
```

Open the terminal UI. It shows your servers and DMs, or asks for a source if it
does not have one yet:

```bash
discord-purge
```

Prefer a browser? Add `--web` to start the web version. A package is optional:

```bash
discord-purge --web
discord-purge --web --package package.zip
```

Both versions use the same layout: **Where** (servers, channels, and DMs),
**Messages** (the open channel), and **Plan** (your selection, preview, and
progress).

## Sign in

To delete messages, the app needs your Discord account token. Save it once:

```bash
discord-purge login
```

The command shows how to find the token. The app checks the token before it
saves it, and all three interfaces use the saved token. Remove it with
`discord-purge login --forget`.

You can also paste the token in the terminal UI or the web panel, or set
`DISCORD_TOKEN`. Treat the token as a password.

## Choose your messages

| Source       | Command                               | Best for                               |
| ------------ | ------------------------------------- | -------------------------------------- |
| Data package | `discord-purge --package package.zip` | Your full history and offline previews |
| One server   | `discord-purge --guild SERVER_ID`     | A server, with no package              |
| One DM       | `discord-purge --channel CHANNEL_ID`  | A DM or group DM, with no package      |

Request your data package in Discord under **Settings → Data & Privacy**. A
package does not include messages sent after the export.

Filter any source:

```bash
discord-purge --package package.zip \
  --content '/example/i' --type text,image --after-date 2025-01-01
```

| Filter                          | Keeps                                                                  |
| ------------------------------- | ---------------------------------------------------------------------- |
| `--content TEXT`                | Messages that contain the text. Case is ignored.                       |
| `--content /pattern/`           | Messages that match a regular expression. Add `i` to ignore case.      |
| `--type LIST`                   | Comma-separated types: `text,media,image,video,audio,voice,file,link`. |
| `--after-date`, `--before-date` | Messages in a `YYYY-MM-DD` range.                                      |
| `--include-pinned`              | Pinned messages too. They are kept by default.                         |

The terminal UI and the web panel also let you:

- Select any mix of servers, channels, and DMs.
- Delete all messages, messages older or newer than one message, or single
  marked messages.
- Paste a Discord link or a DM channel ID to reach a closed DM.

## Delete after review

| Key                   | Action in the terminal UI                           |
| --------------------- | --------------------------------------------------- |
| Tab                   | Go between Where and Messages                       |
| Up/Down, PgUp/PgDn    | Move. Below the last message, load older messages.  |
| Space                 | Select a server, channel, or DM; mark a message     |
| Right or Enter / Left | Expand a server or open a channel / go back         |
| `/`                   | Find by name                                        |
| `o` / `n` / `c`       | Older than / newer than this message / clear range  |
| `l`                   | Paste a Discord link or DM channel ID               |
| `f` / `s` / `r`       | Filters and speed / token or package / reload lists |
| `p`                   | Preview the exact messages                          |
| `d`, then `y`         | Open the confirmation, then delete                  |
| Space / `s`           | Pause or continue / stop, during a run              |
| `?` / `q`             | All keys / quit                                     |

The web panel has the same selections, preview, confirmation, pause, and stop.
A run deletes only the messages in the preview you confirmed. If you change the
selection, the app asks for a new preview.

For scripts, a plain command deletes only with `--execute`:

```bash
DISCORD_TOKEN=... discord-purge --package package.zip --no-tui --execute
```

Ctrl-C stops the run and writes its report. Use `--report PATH` to save the
report.

## Speed and saved progress

Deleting takes time, because Discord's limits control the speed. The defaults
suit most runs:

| Option      | Range     | Default | Effect                                     |
| ----------- | --------- | ------: | ------------------------------------------ |
| `--workers` | 1 to 16   |       4 | Channels deleted at the same time.         |
| `--delay`   | 0 to 3600 |     1.1 | Base seconds between deletes in a channel. |
| `--jitter`  | 0 to 1    |     0.4 | Random change to the delay, as a fraction. |

The app saves completed messages for each account, so you can stop at any time
and continue later. Set `DISCORD_PURGE_STATE_DIR` to keep progress and reports
in a different folder.

## Limits

- Your network must allow UDP, because the app connects over HTTP/3.
- "All listed DMs" means the DMs that Discord returns. A data package can
  include closed DMs.
- A server search includes only the thread messages that Discord search can
  find.
- A package preview count is a maximum, because a package cannot show the
  current pin status or access. The app checks each message before it deletes it.
- The app deletes messages only. It does not remove your reactions.

## Performance

Package previews stay fast and use little memory, also for a long history:

| Preview          | Messages | Time     | Peak memory |
| ---------------- | -------: | -------: | ----------: |
| JSON, all        |  100,000 |  25.2 ms |    13.3 MiB |
| JSON, all        |  200,000 |  42.5 ms |    16.4 MiB |
| JSON, image only |  200,000 |  44.3 ms |    15.0 MiB |
| ZIP, all         |  200,000 |  61.5 ms |    14.7 MiB |

The app is a 5.41 MiB executable, or 2.82 MiB with gzip. Release binaries use
profile-guided optimization, except on Windows ARM64, where Rust has no
profiling runtime.

These results use the macOS ARM64 release binary on an Apple M4 with macOS
27.0.1. Each package has one channel, and the 200,000-message JSON file is
25.7 MiB. Times are the median of 31 runs, and memory is the median of 9 runs.
Network latency and Discord's limits control the speed of real deletion.

## Development

Use Rust stable, [cargo-nextest](https://nexte.st), [just](https://just.systems),
and [uv](https://docs.astral.sh/uv/):

```bash
just test           # Rust and documentation tests
just check          # All CI checks
just release-test   # Installer and archive tests
just release-build  # Native PGO build and archive in target/distrib
just pgo-test       # PGO helper tests
just pgo-build      # PGO build, with an optional target
just pgo-bench      # Compare standard and PGO builds
```

The tests use local servers and invented data. They never send a delete request
to Discord.

## Release

1. Set `[workspace.package].version` in `Cargo.toml`.
2. Add release notes to `CHANGELOG.md`.
3. Run `cargo check` to update `Cargo.lock`.
4. Run `just check`.
5. Commit the version, lock file, and changelog on `main`.
6. Push a matching tag: `git tag v0.0.1 && git push origin v0.0.1`.

The release workflow publishes the GitHub release only after all eight macOS,
Linux, and Windows targets succeed. It then updates the Homebrew formula.

## License

MIT.
