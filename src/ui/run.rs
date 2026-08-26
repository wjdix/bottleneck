//! Terminal setup, the event loop, and the background refresh task.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::github::GitHub;
use crate::snapshot::Snapshot;
use crate::store::Store;
use crate::sync;
use crate::ui::app::{Action, App, SyncStatus, handle_key};
use crate::ui::views;

/// Messages from the refresh task to the UI loop.
enum Message {
    Stage(String),
    Done(Box<Snapshot>),
    Failed(String),
}

/// How long to block waiting for input before redrawing.
///
/// Redrawing on a timeout keeps relative times ("data 4m old", "waiting 2h") ticking and lets
/// sync progress appear without the user pressing anything.
const TICK: Duration = Duration::from_millis(250);

pub async fn run(cfg: Config, store: Store, snapshot: Option<Snapshot>) -> Result<()> {
    let mut terminal = setup()?;
    let result = event_loop(&mut terminal, cfg, Arc::new(tokio::sync::Mutex::new(store)), snapshot).await;
    // Restore the terminal even if the loop failed, so an error message is readable.
    restore(&mut terminal)?;
    result
}

fn setup() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    install_panic_hook();
    Terminal::new(CrosstermBackend::new(out)).context("creating terminal")
}

fn restore(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    Ok(())
}

/// Leave the terminal usable if we panic mid-render.
///
/// Without this a panic inside a draw call leaves raw mode on and the alternate screen active,
/// which makes the shell appear broken and hides the panic message itself.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_raw_mode().ok();
        execute!(io::stdout(), LeaveAlternateScreen).ok();
        previous(info);
    }));
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cfg: Config,
    store: Arc<tokio::sync::Mutex<Store>>,
    snapshot: Option<Snapshot>,
) -> Result<()> {
    let mut app = App::new(cfg.clone(), snapshot);
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    loop {
        terminal.draw(|f| views::draw(f, &app)).context("drawing")?;

        // Drain anything the refresh task has sent.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Message::Stage(s) => app.status = SyncStatus::Running(s),
                Message::Done(snap) => {
                    app.set_snapshot(*snap);
                    app.status = SyncStatus::Idle;
                }
                Message::Failed(e) => app.status = SyncStatus::Failed(e),
            }
        }

        // Poll rather than block, so refresh progress still lands while the user is idle.
        if event::poll(TICK).context("polling for input")? {
            match event::read().context("reading input")? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                    match handle_key(&mut app, key) {
                        Action::Quit => break,
                        Action::Refresh => {
                            // Ignore the request while a sync is already running: a second
                            // concurrent fetch would only duplicate work and race on the cache.
                            // The store is shared rather than moved so that every later refresh
                            // still has something to write to.
                            if !app.is_syncing() {
                                app.status = SyncStatus::Running("starting".into());
                                let tx = tx.clone();
                                let cfg = cfg.clone();
                                let repo = cfg.repos.first().cloned();
                                let store = Arc::clone(&store);
                                tokio::spawn(async move {
                                    let mut guard = store.lock().await;
                                    if let Err(e) =
                                        refresh_once(&mut guard, &cfg, repo, &tx).await
                                    {
                                        let _ = tx.send(Message::Failed(format!("{e:#}")));
                                    }
                                });
                            }
                        }
                        Action::None => {}
                    }
                }
                // A resize just needs a redraw, which happens at the top of the loop.
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

async fn refresh_once(
    store: &mut Store,
    cfg: &Config,
    repo: Option<crate::config::RepoConfig>,
    tx: &mpsc::UnboundedSender<Message>,
) -> Result<()> {
    let repo = repo.context("no repository configured")?;
    let gh = GitHub::new(GitHub::resolve_token().await?)?;

    let tx2 = tx.clone();
    let snap = sync::refresh(&gh, store, &repo, cfg, |stage| {
        let _ = tx2.send(Message::Stage(stage.to_string()));
    })
    .await?;

    let _ = tx.send(Message::Done(Box::new(snap)));
    Ok(())
}
