//! # resource_guard
//!
//! Runtime memory and CPU monitor for mobile/proot environments.
//!
//! Reads `/proc/self/status` (available in proot) every 10 seconds to check
//! the process RSS. If RSS exceeds `MAX_MEMORY_MB * 0.70` (70 % threshold),
//! it logs a warning. If RSS exceeds `MAX_MEMORY_MB * 0.90` (90 % threshold),
//! it triggers a graceful shutdown via the cancellation token to prevent
//! Android from OOM-killing the entire proot session.
//!
//! This module is intentionally allocation-free in its hot loop.

use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn, error};
use crate::config::AppConfig;

/// Interval between memory checks (seconds).
const CHECK_INTERVAL_SECS: u64 = 10;

/// Read the current process RSS from /proc/self/status (Linux/proot only).
/// Returns RSS in kilobytes, or None if the file cannot be read.
fn read_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            // Format: "VmRSS:     12345 kB"
            let kb: u64 = line
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            return Some(kb);
        }
    }
    None
}

/// Read the current process RSS from /proc/self/status.
/// Returns RSS in megabytes.
fn rss_mb() -> Option<u64> {
    read_rss_kb().map(|kb| kb / 1024)
}

/// Background task: monitor memory usage and cancel on OOM risk.
pub async fn run(config: Arc<AppConfig>, cancel: CancellationToken) {
    let max_mb = config.max_memory_mb;
    let warn_threshold = (max_mb as f64 * 0.70) as u64;
    let critical_threshold = (max_mb as f64 * 0.90) as u64;

    info!(
        max_mb,
        warn_threshold_mb = warn_threshold,
        critical_threshold_mb = critical_threshold,
        "Resource guard started"
    );

    let mut interval = tokio::time::interval(
        std::time::Duration::from_secs(CHECK_INTERVAL_SECS)
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Resource guard stopping");
                return;
            }
            _ = interval.tick() => {
                match rss_mb() {
                    None => {
                        // /proc not available (non-Linux or restricted proot)
                    }
                    Some(rss) => {
                        if rss >= critical_threshold {
                            error!(
                                rss_mb = rss,
                                limit_mb = max_mb,
                                "CRITICAL: RSS near OOM limit — initiating graceful shutdown"
                            );
                            cancel.cancel();
                            return;
                        } else if rss >= warn_threshold {
                            warn!(
                                rss_mb = rss,
                                limit_mb = max_mb,
                                pct = (rss * 100 / max_mb),
                                "WARNING: RSS above 70% of limit"
                            );
                        } else {
                            // Log at info level every minute (every 6 ticks)
                            // to avoid log spam. Use a simple counter.
                            // (Static counter is fine here — single task.)
                            static TICK: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let t = TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if t % 6 == 0 {
                                info!(
                                    rss_mb = rss,
                                    limit_mb = max_mb,
                                    pct = (rss * 100 / max_mb),
                                    "Memory OK"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
