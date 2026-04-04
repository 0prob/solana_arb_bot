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
    
    // Initialize TUI state and channel
    let (_tui_tx, tui_rx) = mpsc::channel(100);
    let tui_state = Arc::new(std::sync::Mutex::new(tui::TuiState::new()));

    // Standard tracing initialization
    tracing_subscriber::fmt::init();

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

    let tui_handle = {
        let state = tui_state.clone();
        tokio::spawn(async move {
            if let Err(e) = tui::run_tui(state, tui_rx).await {
                eprintln!("TUI error: {}", e);
            }
        })
    };

    info!("Bot started");

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tui_handle => { cancel.cancel(); }
        r = &mut listener_handle  => { if let Err(e) = r { error!(error = %e, "Listener panicked"); } }
        r = &mut scanner_handle   => { if let Err(e) = r { error!(error = %e, "Scanner panicked"); } }
        r = &mut executor_handle  => { if let Err(e) = r { error!(error = %e, "Executor panicked"); } }
    }

    cancel.cancel();
    let _ = tokio::join!(listener_handle, scanner_handle, executor_handle);
    
    info!("Bot stopped");
    Ok(())
}
