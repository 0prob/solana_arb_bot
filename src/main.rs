mod config;
mod dex_registry;
mod executor;
mod flash_loan;
mod jito;
mod jupiter;
mod listener;
mod safety;
mod scanner;
mod tui;
mod tui_logger;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use config::{AppConfig, CliArgs};

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = CliArgs::parse();

    // Initialize default crypto provider for rustls.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // ── Extract TUI options before args is consumed ───────────────────────
    let no_tui    = args.no_tui;
    let tui_fps   = args.tui_fps.clamp(1, 60);
    let tui_mouse = args.tui_mouse;
    let tui_compact = args.tui_compact;

    // ── TUI event channel ─────────────────────────────────────────────────
    // Capacity of 512 gives plenty of headroom for burst log traffic without
    // blocking the hot-path scanner/executor tasks.
    let (tui_tx, tui_rx) = mpsc::channel::<tui::events::TuiEvent>(512);

    // ── Tracing setup ─────────────────────────────────────────────────────
    use tracing_subscriber::prelude::*;
    let tui_layer = tui_logger::TuiLoggerLayer::new(tui_tx);

    if no_tui {
        // Headless mode: log to stdout only.
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .init();
    } else {
        // TUI mode: suppress fmt output so it doesn't corrupt the terminal,
        // and route everything through the TUI layer.
        tracing_subscriber::registry()
            .with(tui_layer)
            .init();
    }

    let config = Arc::new(AppConfig::from_cli(args)?);
    let cancel = CancellationToken::new();

    {
        let sig_cancel = cancel.clone();
        tokio::spawn(async move {
            safety::await_shutdown_signal().await;
            sig_cancel.cancel();
        });
    }

    let (migration_tx, migration_rx) = mpsc::channel(listener::MIGRATION_CHANNEL_CAPACITY);
    let (opportunity_tx, opportunity_rx) = mpsc::channel(scanner::OPPORTUNITY_CHANNEL_CAPACITY);

    let mut listener_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = listener::run(cfg, migration_tx, cancel).await {
                error!(error = %e, "Listener fatal error");
            }
        })
    };

    let mut scanner_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = scanner::run(cfg, migration_rx, opportunity_tx, cancel).await {
                error!(error = %e, "Scanner fatal error");
            }
        })
    };

    let mut executor_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = executor::run(cfg, opportunity_rx, cancel).await {
                error!(error = %e, "Executor fatal error");
            }
        })
    };

    info!("Bot started");

    if no_tui {
        // Headless: just wait for cancellation or task failure.
        tokio::select! {
            _ = cancel.cancelled() => {}
            r = &mut listener_handle  => { if let Err(e) = r { error!(error = %e, "Listener panicked"); } }
            r = &mut scanner_handle   => { if let Err(e) = r { error!(error = %e, "Scanner panicked"); } }
            r = &mut executor_handle  => { if let Err(e) = r { error!(error = %e, "Executor panicked"); } }
        }
    } else {
        // TUI mode: the TUI task drives shutdown when the user presses q.
        let tui_cancel = cancel.clone();
        let tui_handle = tokio::spawn(async move {
            if let Err(e) = tui::run_tui(tui_rx, tui_cancel, tui_fps, tui_mouse, tui_compact).await {
                eprintln!("TUI error: {e}");
            }
        });

        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tui_handle => { cancel.cancel(); }
            r = &mut listener_handle  => { if let Err(e) = r { error!(error = %e, "Listener panicked"); } }
            r = &mut scanner_handle   => { if let Err(e) = r { error!(error = %e, "Scanner panicked"); } }
            r = &mut executor_handle  => { if let Err(e) = r { error!(error = %e, "Executor panicked"); } }
        }
    }

    cancel.cancel();
    let _ = tokio::join!(listener_handle, scanner_handle, executor_handle);

    info!("Bot stopped");
    Ok(())
}
