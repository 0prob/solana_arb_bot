mod config;
mod dex_registry;
mod executor;
mod flash_loan;
mod jito;
mod jupiter;
mod listener;
mod resource_guard;
mod safety;
mod scanner;
#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "tui")]
mod tui_logger;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use config::{AppConfig, CliArgs};

// ── Tokio runtime: 2 worker threads for mobile/proot efficiency ──────────────
// On ARM64 proot the scheduler overhead per-thread is high; 2 threads is the
// sweet spot for I/O-bound workloads on a 4-core mobile SoC.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = CliArgs::parse();

    // Initialize default crypto provider for rustls.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // ── Extract TUI options before args is consumed ───────────────────────
    let no_tui    = args.no_tui;
    #[cfg(feature = "tui")]
    let tui_fps   = args.tui_fps.clamp(1, 10); // cap at 10 fps for mobile
    #[cfg(feature = "tui")]
    let tui_mouse = args.tui_mouse;
    #[cfg(feature = "tui")]
    let tui_compact = args.tui_compact;

    // ── Tracing setup ─────────────────────────────────────────────────────
    #[cfg(feature = "tui")]
    let tui_tx_opt: Option<mpsc::Sender<tui::events::TuiEvent>>;

    #[cfg(feature = "tui")]
    let tui_rx_opt: Option<mpsc::Receiver<tui::events::TuiEvent>>;

    // Bounded TUI channel: 64 events max to prevent log-storm OOM
    #[cfg(feature = "tui")]
    {
        let (tx, rx) = mpsc::channel::<tui::events::TuiEvent>(64);
        tui_tx_opt = Some(tx);
        tui_rx_opt = Some(rx);
    }

    use tracing_subscriber::prelude::*;

    if no_tui {
        // Headless mode: minimal structured log to stdout only.
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(false),
            )
            .with(tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sb=info".parse().unwrap())
                .add_directive("warn".parse().unwrap()))
            .init();
    } else {
        #[cfg(feature = "tui")]
        {
            let tui_layer = tui_logger::TuiLoggerLayer::new(tui_tx_opt.as_ref().unwrap().clone());
            tracing_subscriber::registry()
                .with(tui_layer)
                .with(tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("sb=info".parse().unwrap())
                    .add_directive("warn".parse().unwrap()))
                .init();
        }
        #[cfg(not(feature = "tui"))]
        {
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_target(false),
                )
                .with(tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("sb=info".parse().unwrap())
                    .add_directive("warn".parse().unwrap()))
                .init();
        }
    }

    let config = Arc::new(AppConfig::from_cli(args)?);
    let cancel = CancellationToken::new();

    // ── Resource guard: monitors RAM and triggers graceful degradation ────
    {
        let rg_cancel = cancel.clone();
        let rg_cfg = config.clone();
        tokio::spawn(async move {
            resource_guard::run(rg_cfg, rg_cancel).await;
        });
    }

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

    info!("Bot started (mobile-optimized build)");

    if no_tui {
        // Headless: wait for cancellation or task failure.
        // If any critical task panics, cancel all others for a clean shutdown.
        tokio::select! {
            _ = cancel.cancelled() => {}
            r = &mut listener_handle  => {
                if let Err(e) = r { error!(error = %e, "Listener panicked"); }
                else { error!("Listener exited unexpectedly"); }
                cancel.cancel();
            }
            r = &mut scanner_handle   => {
                if let Err(e) = r { error!(error = %e, "Scanner panicked"); }
                else { error!("Scanner exited unexpectedly"); }
                cancel.cancel();
            }
            r = &mut executor_handle  => {
                if let Err(e) = r { error!(error = %e, "Executor panicked"); }
                else { error!("Executor exited unexpectedly"); }
                cancel.cancel();
            }
        }
    } else {
        #[cfg(feature = "tui")]
        {
            // TUI mode: the TUI task drives shutdown when the user presses q.
            let tui_cancel = cancel.clone();
            let tui_rx = tui_rx_opt.expect("tui_rx must be Some in TUI mode");
            let tui_handle = tokio::spawn(async move {
                if let Err(e) = tui::run_tui(tui_rx, tui_cancel, tui_fps, tui_mouse, tui_compact).await {
                    eprintln!("TUI error: {e}");
                }
            });

            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tui_handle => { cancel.cancel(); }
                r = &mut listener_handle  => {
                    if let Err(e) = r { error!(error = %e, "Listener panicked"); }
                    else { error!("Listener exited unexpectedly"); }
                    cancel.cancel();
                }
                r = &mut scanner_handle   => {
                    if let Err(e) = r { error!(error = %e, "Scanner panicked"); }
                    else { error!("Scanner exited unexpectedly"); }
                    cancel.cancel();
                }
                r = &mut executor_handle  => {
                    if let Err(e) = r { error!(error = %e, "Executor panicked"); }
                    else { error!("Executor exited unexpectedly"); }
                    cancel.cancel();
                }
            }
        }
        #[cfg(not(feature = "tui"))]
        {
            // TUI feature disabled at compile time — fall back to headless.
            tokio::select! {
                _ = cancel.cancelled() => {}
                r = &mut listener_handle  => {
                    if let Err(e) = r { error!(error = %e, "Listener panicked"); }
                    else { error!("Listener exited unexpectedly"); }
                    cancel.cancel();
                }
                r = &mut scanner_handle   => {
                    if let Err(e) = r { error!(error = %e, "Scanner panicked"); }
                    else { error!("Scanner exited unexpectedly"); }
                    cancel.cancel();
                }
                r = &mut executor_handle  => {
                    if let Err(e) = r { error!(error = %e, "Executor panicked"); }
                    else { error!("Executor exited unexpectedly"); }
                    cancel.cancel();
                }
            }
        }
    }

    cancel.cancel();
    let _ = tokio::join!(listener_handle, scanner_handle, executor_handle);

    info!("Bot stopped");
    Ok(())
}
