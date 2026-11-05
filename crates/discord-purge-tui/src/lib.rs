//! Terminal workspace for the shared engine: select where and which messages,
//! preview the exact set, confirm, and watch the run.
mod app;
mod ui;

use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use discord_purge_core::Options;
use futures::StreamExt;

use app::{App, Screen};

pub async fn run(config: Options) -> anyhow::Result<()> {
    let mut terminal = ratatui::try_init()?;
    // Pasted text (a token, a path, a link) arrives as one event.
    if let Err(error) = crossterm::execute!(std::io::stdout(), EnableBracketedPaste) {
        ratatui::restore();
        return Err(error.into());
    }
    let mut app = App::new(config);
    app.start();
    let result = event_loop(&mut terminal, &mut app).await;
    app.cancel_tasks();
    if let Some(controls) = &app.controls {
        controls.stop();
    }
    let paste_mode = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    if let Err(error) = paste_mode {
        eprintln!(
            "warning: cannot turn off bracketed paste: {error}. Run `reset` if pasted text looks wrong."
        );
    }
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }
        tokio::select! {
            _ = ticker.tick() => app.tick().await,
            event = events.next() => match event {
                Some(Ok(Event::Paste(text))) => app.paste(&text),
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    let size = terminal.size()?;
                    let small = size.width < ui::MIN_WIDTH || size.height < ui::MIN_HEIGHT;
                    let interrupt = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                    if interrupt || (small && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))) {
                        // Stop a run first; quit otherwise.
                        match (&app.controls, app.screen) {
                            (Some(controls), Screen::Running) => controls.stop(),
                            _ => app.should_quit = true,
                        }
                    } else if !small {
                        app.key(key.code);
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
                _ => {}
            },
        }
    }
}
