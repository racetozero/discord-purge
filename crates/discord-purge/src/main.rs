//! discord-purge command line.
//!
//! Three modes: a terminal UI (default), a headless run (`--no-tui`), and a
//! local web panel (`--web`). Every mode shares the same engine and defaults to
//! a dry run; a real deletion needs `--execute` (headless) or an explicit
//! confirmation (TUI/web).

mod update;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use discord_purge_core::api::DiscordClient;
use discord_purge_core::engine::{Engine, EngineConfig, Stats};
use discord_purge_core::filter::Filter;
use discord_purge_core::resume::format_duration;
use discord_purge_core::{Options, Source, prepare, resume};

#[derive(Parser, Debug)]
#[command(
    name = "discord-purge",
    version,
    about = "Delete your own Discord messages (TUI + web panel).",
    long_about = "Delete your own Discord messages from a data package or live search.\n\
    Deletes ONLY the token owner's own messages. Automating a user account breaks\n\
    Discord's Terms of Service and can get it banned. Dry run is the default."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Your user token. Also read from the DISCORD_TOKEN environment variable.
    /// Without it, the saved token is used. A token that Discord accepts in the
    /// CLI, the TUI, or the web panel is saved for all of them.
    #[arg(long, env = "DISCORD_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Keep a new token in memory only. The saved token is still used when no
    /// token is given.
    #[arg(long)]
    no_save_token: bool,

    /// Path to a data package .zip or an extracted folder. When set, the
    /// package is the source and no search happens.
    #[arg(long)]
    package: Option<PathBuf>,

    /// Live-search a server by guild id (instead of a package).
    #[arg(long)]
    guild: Option<String>,

    /// Live-search a DM or group DM by channel id (instead of a package).
    #[arg(long)]
    channel: Option<String>,

    /// Only messages whose text contains this substring, or a regex in slashes:
    /// /pattern/ or /pattern/i
    #[arg(long)]
    content: Option<String>,

    /// Comma-separated types to keep: text,media,image,video,audio,voice,file,link
    #[arg(long)]
    r#type: Option<String>,

    /// Only messages after this date (YYYY-MM-DD).
    #[arg(long)]
    after_date: Option<String>,

    /// Only messages before this date (YYYY-MM-DD).
    #[arg(long)]
    before_date: Option<String>,

    /// Also delete pinned messages (off by default).
    #[arg(long)]
    include_pinned: bool,

    /// Run the local web panel at http://127.0.0.1:8787 instead of the TUI.
    #[arg(long)]
    web: bool,

    /// Port for the web panel.
    #[arg(long, default_value_t = 8787)]
    port: u16,

    /// Plain non-interactive run (no TUI).
    #[arg(long)]
    no_tui: bool,

    /// Start a real deletion instead of a dry run (headless only).
    #[arg(long)]
    execute: bool,

    /// Channels deleted in parallel.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Base seconds between deletes within one channel.
    #[arg(long, default_value_t = 1.1)]
    delay: f64,

    /// +/- jitter fraction on the delay.
    #[arg(long, default_value_t = 0.4)]
    jitter: f64,

    /// Write the end-of-run report to this path.
    #[arg(long)]
    report: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Prepare process headers in the private identity cache.
    Identity {
        /// Fetch public build metadata, bootstrap installation, and verify HTTP/3.
        #[arg(long)]
        check: bool,
    },
    /// Save your Discord token for later runs, and show how to find it.
    Login {
        /// Provide the token directly instead of pasting it when asked. Also
        /// read from DISCORD_TOKEN.
        #[arg(long, env = "DISCORD_TOKEN", hide_env_values = true)]
        token: Option<String>,

        /// Forget the saved token instead of saving one.
        #[arg(long)]
        forget: bool,
    },
    /// Update to the latest release.
    Update,
}

/// Handle `discord-purge login`: show the how-to, save a verified token, or
/// forget the saved one. A token is saved only after it verifies, so a typo is
/// never stored.
async fn do_login(token: Option<String>, forget: bool) -> anyhow::Result<()> {
    if forget {
        if resume::forget_token()? {
            println!("Saved token removed.");
        } else {
            println!("No saved token to remove.");
        }
        return Ok(());
    }

    println!("{}\n", discord_purge_core::TOKEN_HELP);

    // Hide terminal input. Piped input uses the normal line reader.
    let token = match token.filter(|t| !t.trim().is_empty()) {
        Some(t) => t,
        None => {
            use std::io::Write;
            print!("Paste your token and press Enter: ");
            if let Err(error) = std::io::stdout().flush() {
                eprintln!(
                    "warning: cannot show the prompt: {error}. You can still paste the token."
                );
            }
            let mut line = String::new();
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
                crossterm::terminal::enable_raw_mode()?;
                let input = (|| -> anyhow::Result<()> {
                    loop {
                        let Event::Key(key) = read()? else {
                            continue;
                        };
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        let interrupt = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        anyhow::ensure!(
                            !interrupt && key.code != KeyCode::Esc,
                            "Login was cancelled. Run login again when you are ready."
                        );
                        match key.code {
                            KeyCode::Enter => return Ok(()),
                            KeyCode::Char(character) if !character.is_control() => {
                                line.push(character)
                            }
                            KeyCode::Backspace => {
                                line.pop();
                            }
                            _ => {}
                        }
                    }
                })();
                crossterm::terminal::disable_raw_mode()?;
                println!();
                input?;
            } else {
                std::io::stdin().read_line(&mut line)?;
            }
            line.trim().to_string()
        }
    };
    if token.is_empty() {
        anyhow::bail!(
            "no token was given. Pass `login --token <token>`, set DISCORD_TOKEN, or paste one when asked."
        );
    }

    let client = DiscordClient::new(token.clone())?;
    let me = client.current_user().await.map_err(|e| {
        anyhow::anyhow!(
            "that token did not verify ({e}). Copy the \"authorization\" header value exactly, with no quotes. Nothing was saved."
        )
    })?;
    let path = resume::save_token(&token)?;
    println!("Verified and signed in as {}.", me.label());
    println!(
        "Token saved to {}. Future runs need no --token.",
        path.display()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings = EngineConfig::new(cli.workers, cli.delay, cli.jitter)?;
    anyhow::ensure!(
        cli.package.is_none() || (cli.guild.is_none() && cli.channel.is_none()),
        "You combined a package with live search. Use --package, or use --guild and/or --channel."
    );
    let filter = Filter::default().with_inputs(
        cli.content.as_deref(),
        cli.r#type.as_deref(),
        cli.after_date.as_deref(),
        cli.before_date.as_deref(),
    )?;

    match &cli.command {
        Some(Command::Login { token, forget }) => return do_login(token.clone(), *forget).await,
        Some(Command::Update) => return update::run().await,
        Some(Command::Identity { check }) => {
            let path = discord_purge_core::identity::initialize().await?;
            if *check {
                DiscordClient::new("")?.check_connection().await?;
            }
            println!("Process identity cache: {}", path.display());
            if *check {
                println!(
                    "Identity cache prepared. HTTP/3 check passed. No account token was sent."
                );
            }
            return Ok(());
        }
        None => {}
    }

    anyhow::ensure!(
        !(cli.web && cli.no_tui),
        "You combined --web and --no-tui. Use --web for the panel, or --no-tui for a plain run."
    );
    anyhow::ensure!(
        cli.no_tui || !cli.execute,
        "You used --execute without --no-tui. --execute applies only to a plain run. Add --no-tui, or remove --execute and confirm the deletion in the terminal UI or web panel."
    );
    anyhow::ensure!(
        cli.no_tui || cli.report.is_none(),
        "You used --report without --no-tui. --report applies only to a plain run. Add --no-tui, or remove --report to save the report in the state directory."
    );

    // Prepare one launch identity before online frontends start. Offline package
    // previews keep their no-network, no-state behavior.
    if cli.web || cli.package.is_none() || cli.execute {
        discord_purge_core::identity::initialize().await?;
    }

    let token_store = if cli.no_save_token {
        resume::TokenStore::Read
    } else {
        resume::TokenStore::ReadWrite
    };
    let cli_token = cli.token.clone().filter(|t| !t.trim().is_empty());

    if !cli.no_tui {
        // The front ends read the saved token when they need it, so a token
        // that another front end saves while this one runs is used too.
        let options = Options {
            token: cli_token,
            token_store,
            package: cli.package.clone(),
            guild: cli.guild.clone(),
            channel: cli.channel.clone(),
            filter,
            settings,
            include_pinned: cli.include_pinned,
        };
        return if cli.web {
            discord_purge_web::serve(options, cli.port).await
        } else {
            discord_purge_tui::run(options).await
        };
    }

    // Headless: an explicit --token or DISCORD_TOKEN wins over the saved token.
    let token = cli_token.or_else(|| token_store.load());
    // Save the token once Discord accepts it, so the TUI and web panel use it.
    // A failed save only warns: it must not stop a preview or a run.
    let remember = |token: &Option<String>| match token.as_deref().map(|t| token_store.remember(t))
    {
        Some(Ok(true)) => println!(
            "Token saved to {}. The TUI and web panel use it too.",
            resume::token_path().display()
        ),
        Some(Err(e)) => eprintln!("warning: {e}"),
        _ => {}
    };

    // A dry-run preview of a package works with no token (nothing is
    // deleted and no account is needed). A real run, or any live search, needs
    // one.
    if token.is_none() && (cli.execute || cli.package.is_none()) {
        anyhow::bail!(
            "no token. Pass --token <token> or set DISCORD_TOKEN. A token-free run is allowed only for a --package dry run (no --execute)."
        );
    }
    let client =
        DiscordClient::new(token.clone().unwrap_or_default())?.include_pinned(cli.include_pinned);
    let source = Source::from_inputs(
        cli.package.clone(),
        cli.guild.clone(),
        cli.channel.clone(),
        cli.include_pinned,
    )?;

    let preview = prepare(&client, &source, &filter).await?;
    if matches!(source, Source::Search(_)) {
        remember(&token);
    }
    println!(
        "Matched {} message(s) across {} channel(s).",
        preview.total,
        preview.jobs.len()
    );
    for job in preview.jobs.iter().take(50) {
        println!("  {:<40} {}", job.label, job.items.len());
    }
    if preview.jobs.len() > 50 {
        println!("  … and {} more channel(s)", preview.jobs.len() - 50);
    }
    if !preview.complete {
        println!("{}", discord_purge_core::search::INCOMPLETE_NOTICE);
    }

    if !cli.execute {
        println!("\nDry run: nothing was deleted. Re-run with --execute to delete.");
        return Ok(());
    }
    if preview.total == 0 {
        println!("Nothing to delete.");
        return Ok(());
    }

    let run = preview.confirm(&client).await?;
    remember(&token);
    if run.already_done > 0 {
        println!(
            "Skipping {} already-deleted message(s) from the resume log.",
            run.already_done
        );
    }
    println!("Deleting {} message(s)…", run.total);

    let account = run.account_label.clone();
    let stats = Stats::new(run.total, settings.workers);
    let (engine, controls) = Engine::new(settings, client, stats.clone());
    let log = resume::ResumeLog::open(&run.account_id)?;

    let signal = tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => controls.stop(),
            Err(error) => eprintln!("warning: Ctrl-C cannot stop this run: {error}."),
        }
    });

    let printer = tokio::spawn({
        let stats = stats.clone();
        async move {
            use std::io::Write;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let snapshot = stats.snapshot();
                let mut stderr = std::io::stderr();
                // A closed pipe ends only the progress line. It must not stop the deletion run.
                let written = write!(
                    stderr,
                    "\r  {}/{}  deleted {}  skipped {}  deferred {}  failed {}  {:.2}/s   ",
                    snapshot.processed,
                    snapshot.total,
                    snapshot.deleted,
                    snapshot.skipped,
                    snapshot.deferred,
                    snapshot.failed,
                    snapshot.rate
                )
                .and_then(|()| stderr.flush());
                if written.is_err() || snapshot.phase.is_finished() {
                    break;
                }
            }
        }
    });

    engine.run(run, move |key| log.record(key)).await;
    printer.abort();
    signal.abort();

    let snap = stats.snapshot();
    // Write the report first: printing can panic on a closed pipe.
    let report = resume::write_report(&account, &snap, cli.report.as_deref())?;
    eprintln!();
    println!(
        "{}. deleted {} | skipped {} | deferred {} | failed {} in {}.",
        snap.phase.label(),
        snap.deleted,
        snap.skipped,
        snap.deferred,
        snap.failed,
        format_duration(snap.elapsed)
    );
    if let Some(notice) = snap.limit_notice() {
        println!("{notice}");
    }
    println!("Report: {}", report.display());
    anyhow::ensure!(
        snap.complete(),
        "Deletion did not complete. Read the report at {} and retry the remaining messages after you correct the error.",
        report.display()
    );
    Ok(())
}
