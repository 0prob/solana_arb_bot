//! TUI module — production-grade ratatui 0.29 + crossterm 0.28 terminal UI.
//!
//! # Architecture
//! The TUI runs in its own Tokio task and owns all terminal state.  It
//! communicates with the rest of the bot exclusively through an
//! `mpsc::Receiver<TuiEvent>` channel, so the hot-path scanner/executor
//! tasks are never blocked by rendering.
//!
//! The event loop uses `tokio::select!` over three branches:
//! 1. `CancellationToken` — graceful shutdown from the main loop.
//! 2. `crossterm::event::EventStream` — keyboard / mouse / resize events.
//! 3. `mpsc::Receiver<TuiEvent>` — structured updates from the bot.
//!
//! A `tokio::time::interval` drives the FPS limiter; rendering only
//! happens on that tick, not on every event, so CPU usage stays minimal.

pub mod app;
pub mod events;
pub mod ui;
pub mod widgets;

use std::{
    io,
    panic,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use app::{ActiveTab, App};
use events::TuiEvent;

/// Run the TUI until the user quits or the cancellation token is fired.
///
/// # Parameters
/// - `rx`: channel receiver for structured events from the rest of the bot.
/// - `cancel`: shared cancellation token; the TUI cancels it on quit so the
///   whole bot shuts down cleanly.
/// - `fps`: target render rate (frames per second).
/// - `mouse_enabled`: whether to enable mouse capture on startup.
/// - `compact`: force compact layout regardless of terminal size.
pub async fn run_tui(
    mut rx: mpsc::Receiver<TuiEvent>,
    cancel: CancellationToken,
    fps: u64,
    mouse_enabled: bool,
    compact: bool,
) -> Result<()> {
    // ── Terminal setup ────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if mouse_enabled {
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    } else {
        execute!(stdout, EnterAlternateScreen)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Install a panic hook that restores the terminal before printing the
    // panic message, preventing a garbled terminal on unexpected crashes.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));

    let result = event_loop(&mut terminal, &mut rx, &cancel, fps, mouse_enabled, compact).await;

    // ── Terminal teardown ─────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Inner event loop — separated so teardown always runs even on error.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: &mut mpsc::Receiver<TuiEvent>,
    cancel: &CancellationToken,
    fps: u64,
    mouse_enabled: bool,
    compact: bool,
) -> Result<()> {
    let mut app = App::new(mouse_enabled, compact);
    let tick_interval = Duration::from_millis(1000 / fps.max(1));
    let mut ticker = tokio::time::interval(tick_interval);
    // Skip missed ticks rather than bursting to catch up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut event_stream = EventStream::new();

    loop {
        tokio::select! {
            // ── Graceful shutdown ─────────────────────────────────────────
            _ = cancel.cancelled() => {
                break;
            }

            // ── Render tick ───────────────────────────────────────────────
            _ = ticker.tick() => {
                let t0 = Instant::now();
                terminal.draw(|f| ui::render(f, &app))?;
                app.render_time_us = t0.elapsed().as_micros() as u64;
            }

            // ── Terminal events (keyboard / mouse / resize) ───────────────
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        app.events_processed += 1;
                        if handle_terminal_event(&mut app, event) {
                            // User requested quit — cancel the whole bot.
                            cancel.cancel();
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        app.set_error(format!("Terminal event error: {e}"));
                    }
                    None => break, // EventStream exhausted (terminal closed).
                }
            }

            // ── Bot events (logs, opportunities, bundles) ─────────────────
            maybe_tui_event = rx.recv() => {
                match maybe_tui_event {
                    Some(ev) => handle_tui_event(&mut app, ev),
                    None => break, // All senders dropped — bot has shut down.
                }
            }
        }
    }

    Ok(())
}

/// Handle a crossterm terminal event.  Returns `true` if the TUI should quit.
fn handle_terminal_event(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) => {
            // Quit on q or Ctrl+C.
            if key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return true;
            }
            match key.code {
                // Tab navigation.
                KeyCode::Tab => {
                    app.active_tab = app.active_tab.next();
                }
                KeyCode::BackTab => {
                    app.active_tab = app.active_tab.prev();
                }
                // Direct tab jump.
                KeyCode::Char('1') => app.active_tab = ActiveTab::Dashboard,
                KeyCode::Char('2') => app.active_tab = ActiveTab::Opportunities,
                KeyCode::Char('3') => app.active_tab = ActiveTab::Logs,
                KeyCode::Char('4') => app.active_tab = ActiveTab::Help,
                // Pause / resume.
                KeyCode::Char('p') => {
                    app.paused = !app.paused;
                }
                // Force redraw (handled naturally by the tick, but clear state).
                KeyCode::Char('r') => {
                    app.clear_error();
                }
                // Clear error banner.
                KeyCode::Char('c') => {
                    app.clear_error();
                }
                // Toggle mouse.
                KeyCode::Char('m') => {
                    app.mouse_enabled = !app.mouse_enabled;
                    if app.mouse_enabled {
                        let _ = execute!(io::stdout(), EnableMouseCapture);
                    } else {
                        let _ = execute!(io::stdout(), DisableMouseCapture);
                    }
                }
                // Cycle log filter through some useful presets.
                KeyCode::Char('f') => {
                    app.log_filter = match &app.log_filter {
                        None => Some("ERROR".to_string()),
                        Some(f) if f == "ERROR" => Some("WARN".to_string()),
                        Some(f) if f == "WARN" => Some("opportunity".to_string()),
                        _ => None,
                    };
                    app.log_scroll = 0;
                }
                // Scroll down.
                KeyCode::Down | KeyCode::Char('j') => {
                    match app.active_tab {
                        ActiveTab::Logs => {
                            app.log_scroll = app.log_scroll.saturating_add(1);
                        }
                        ActiveTab::Opportunities => {
                            app.opp_scroll = app.opp_scroll.saturating_add(1);
                        }
                        _ => {}
                    }
                }
                // Scroll up.
                KeyCode::Up | KeyCode::Char('k') => {
                    match app.active_tab {
                        ActiveTab::Logs => {
                            app.log_scroll = app.log_scroll.saturating_sub(1);
                        }
                        ActiveTab::Opportunities => {
                            app.opp_scroll = app.opp_scroll.saturating_sub(1);
                        }
                        _ => {}
                    }
                }
                // Jump to top.
                KeyCode::Char('g') => {
                    app.log_scroll = 0;
                    app.opp_scroll = 0;
                }
                // Jump to bottom (auto-scroll).
                KeyCode::Char('G') => {
                    app.log_scroll = 0; // 0 = auto-scroll to bottom in render.
                    app.opp_scroll = 0;
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) => {
            if app.mouse_enabled {
                match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        app.log_scroll = app.log_scroll.saturating_add(1);
                    }
                    MouseEventKind::ScrollUp => {
                        if app.log_scroll > 0 {
                            app.log_scroll -= 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        Event::Resize(_, _) => {
            // ratatui handles resize automatically on the next draw; no action needed.
        }
        _ => {}
    }
    false
}

/// Apply a structured TuiEvent to the App state.
fn handle_tui_event(app: &mut App, event: TuiEvent) {
    match event {
        TuiEvent::Log { level, target, message } => {
            let lvl = app::LogLevel::from_str(&level);
            app.add_log(lvl, target, message);
        }
        TuiEvent::OpportunityFound { token, loan_sol, profit_sol } => {
            app.add_opportunity(token, loan_sol, profit_sol);
        }
        TuiEvent::BundleSubmitted { bundle_id, profit_sol, tip_sol } => {
            app.add_bundle(bundle_id, profit_sol, tip_sol);
        }
        TuiEvent::CriticalError(msg) => {
            app.set_error(msg);
        }
    }
}
