// src/main.rs
// ═══════════════════════════════════════════════════════════════════════
//  SOL-ARB-BOT — Zero-Capital Solana Flash Loan Arbitrage
//
//  Architecture:
//    Listener (gRPC) ──▶ Scanner (Jupiter quotes) ──▶ Executor (atomic tx)
//
//  Pipeline:
//    1. Listener streams pool creation events from 35+ DEXes via gRPC
//    2. Scanner evaluates profitability via Jupiter V6 quotes
//    3. Executor refreshes quotes, simulates, then submits atomic tx
//    4. Submits via Jito bundle (preferred) or standard RPC fallback
//    5. Flash loan provider auto-selected (JupiterLend → Kamino → Marginfi → Save)
//
//  Performance (v2):
//    • worker_threads raised from 4 → 8 to match 32 concurrent scanner
//      evaluations + executor + listener + background tasks.
//    • Migration channel capacity: 512 (was 128) — absorbs burst events.
//    • Opportunity channel capacity: 64 (was 32) — reduces executor drops.
//    • Quote cache: 500 ms TTL deduplicates burst requests for same token.
//    • ALT cache: 30 s TTL avoids redundant RPC fetches per execution.
//    • Balance cache: 2 s TTL avoids redundant balance checks.
//    • Blockhash cache: ~350 ms TTL avoids redundant blockhash fetches.
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
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;

use config::{AppConfig, CliArgs};

// Worker thread count.
//
// Raised from 4 → 8 to handle the increased concurrency demands of:
//   • SCANNER_MAX_CONCURRENCY=32 (each eval spawns a task)
//   • Executor (1 task)
//   • Listener (1 task, with gRPC stream processing)
//   • Background tasks (metrics reporter, dedupe cleanup)
//
// 8 threads gives each category headroom without over-provisioning.
// For machines with ≥ 8 cores, consider setting this to num_cpus.
#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let args = CliArgs::parse();
    let no_tui = args._no_tui;

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

    // Use the new channel capacities from the listener and scanner modules.
    let (migration_tx, migration_rx) =
        mpsc::channel(listener::MIGRATION_CHANNEL_CAPACITY);
    let (opportunity_tx, opportunity_rx) =
        mpsc::channel(scanner::OPPORTUNITY_CHANNEL_CAPACITY);

    let mut listener_handle = {
        let cfg = config.clone();
        let cancel = cancel.clone();
        let d = dash.clone();
        tokio::spawn(async move {
            if let Err(e) = listener::run(cfg, migration_tx, cancel, d).await {
                error!(error = %e, "Listener fatal error");
            }
        })
    };

    let mut scanner_handle = {
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

    let mut executor_handle = {
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

    // Use mutable references so the JoinHandles are NOT consumed by select!
    // and remain available for the graceful-shutdown join below.
    tokio::select! {
        _ = cancel.cancelled() => {}
        r = &mut listener_handle  => { if let Err(e) = r { error!(error = %e, "Listener panicked"); } }
        r = &mut scanner_handle   => { if let Err(e) = r { error!(error = %e, "Scanner panicked"); } }
        r = &mut executor_handle  => { if let Err(e) = r { error!(error = %e, "Executor panicked"); } }
    }

    // Broadcast shutdown to all subsystems and the dashboard.
    cancel.cancel();
    dash.send(tui::DashEvent::Shutdown);

    // Wait up to 3 s for each task to drain and exit cleanly.  Each task's loop
    // polls cancel.cancelled() so it will exit promptly.  The timeout prevents
    // a hung task (e.g. a blocked RPC call) from stalling the process forever.
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        async {
            let _ = tokio::join!(listener_handle, scanner_handle, executor_handle);
        },
    )
    .await;

    Ok(())
}
