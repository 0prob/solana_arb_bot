// src/main.rs
// ═══════════════════════════════════════════════════════════════════════
//  SOL-ARB-BOT — Zero-Capital Solana Flash Loan Arbitrage
//
//  Architecture:
//    Listener (gRPC) ──▶ Scanner (Jupiter quotes) ──▶ Executor (atomic tx)
//
//  Pipeline:
//    1. Listener streams pool creation events from 25+ DEXes via gRPC
//    2. Scanner evaluates profitability via Jupiter V6 quotes
//    3. Executor refreshes quotes, simulates, then submits atomic tx
//    4. Submits via Jito bundle (preferred) or standard RPC fallback
//    5. Flash loan provider auto-selected (JupiterLend → Kamino → Marginfi → Save)
// ═══════════════════════════════════════════════════════════════════════

mod config;
mod dex_registry;
mod executor;
mod flash_loan;
mod jito;
mod jupiter;
mod listener;
mod logging;
mod metrics;
mod safety;
mod scanner;
mod tatum;
mod tui;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;

use config::{AppConfig, CliArgs};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let args = CliArgs::parse();
    let no_tui = args.no_tui;

    // When running headless (CI, Docker, systemd) or --no-tui is passed,
    // initialize structured tracing to stderr instead of the TUI.
    if no_tui {
        logging::init();
    }

    let config = Arc::new(AppConfig::from_cli(args)?);
    let metrics = metrics::Metrics::new();
    let cancel = CancellationToken::new();

    // Spawn the ratatui dashboard thread (no-op stub when --no-tui).
    let dash = if no_tui {
        tui::spawn_null()
    } else {
        tui::spawn(cancel.clone())
    };

    // Metrics tick → dashboard every 5s.
    metrics::spawn_reporter(metrics.clone(), 5, cancel.clone(), dash.clone());

    // OS signal handlers → cancel token + dashboard notification.
    {
        let sig_cancel = cancel.clone();
        let sig_dash = dash.clone();
        tokio::spawn(async move {
            safety::await_shutdown_signal().await;
            sig_dash.send(tui::DashEvent::Shutdown);
            sig_cancel.cancel();
        });
    }

    let (migration_tx, migration_rx) = mpsc::channel(128);
    let (opportunity_tx, opportunity_rx) = mpsc::channel(32);

    let listener_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        let d = dash.clone();
        tokio::spawn(async move {
            if let Err(e) = listener::run(cfg, migration_tx, cancel, d).await {
                error!(error = %e, "Listener fatal error");
            }
        })
    };

    let scanner_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        let m = metrics.clone();
        let d = dash.clone();
        tokio::spawn(async move {
            if let Err(e) = scanner::run(cfg, migration_rx, opportunity_tx, cancel, m, d).await {
                error!(error = %e, "Scanner fatal error");
            }
        })
    };

    let executor_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        let m = metrics.clone();
        let d = dash.clone();
        tokio::spawn(async move {
            if let Err(e) = executor::run(cfg, opportunity_rx, cancel, m, d).await {
                error!(error = %e, "Executor fatal error");
            }
        })
    };

    tokio::select! {
        _ = cancel.cancelled() => {}
        r = listener_handle  => { if let Err(e) = r { error!(error = %e, "Listener panicked"); } }
        r = scanner_handle   => { if let Err(e) = r { error!(error = %e, "Scanner panicked"); } }
        r = executor_handle  => { if let Err(e) = r { error!(error = %e, "Executor panicked"); } }
    }

    dash.send(tui::DashEvent::Shutdown);
    cancel.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;

    Ok(())
}
