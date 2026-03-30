// src/safety/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Safety module — guards, circuit breakers, deduplication, shutdown.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::signal;
use tracing::{error, info, warn};

// ── Signal handling ─────────────────────────────────────────────────

/// Awaits SIGINT or SIGTERM. Returns when either fires.
pub async fn await_shutdown_signal() {
    #[cfg(unix)]
    {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut term_stream) => {
                tokio::select! {
                    ctrl = signal::ctrl_c() => match ctrl {
                        Ok(())  => info!("Received SIGINT"),
                        Err(e)  => {
                            error!(error = %e, "Ctrl+C handler failed; continuing to wait for SIGTERM");
                            let _ = term_stream.recv().await;
                            info!("Received SIGTERM");
                        }
                    },
                    _ = term_stream.recv() => info!("Received SIGTERM"),
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to install SIGTERM handler; waiting for Ctrl+C only");
                match signal::ctrl_c().await {
                    Ok(())  => info!("Received SIGINT"),
                    Err(ctrl_err) => error!(error = %ctrl_err, "Failed to install Ctrl+C handler"),
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        match signal::ctrl_c().await {
            Ok(())  => info!("Received SIGINT"),
            Err(e)  => error!(error = %e, "Failed to install Ctrl+C handler"),
        }
    }
}

// ── Circuit breaker ─────────────────────────────────────────────────

/// Tracks consecutive failures and trips a cooldown when the threshold is hit.
///
/// Ordering note: `record_failure` uses `AcqRel` so the failure count write
/// is visible to any concurrent reader before `cooldown` is called. Using
/// `Relaxed` here was a correctness issue: a concurrent success reset could
/// race with a failure increment and produce an incorrectly low count under
/// weak memory models.
pub struct CircuitBreaker {
    consecutive_failures: AtomicU64,
    max_failures:         u64,
    cooldown:             std::time::Duration,
}

impl CircuitBreaker {
    pub fn new(max_failures: u64, cooldown_secs: u64) -> Self {
        Self {
            consecutive_failures: AtomicU64::new(0),
            max_failures,
            cooldown: std::time::Duration::from_secs(cooldown_secs),
        }
    }

    pub fn record_success(&self) {
        // SeqCst ensures any in-flight failure increment is fully visible before
        // we zero the counter, preventing a spurious post-reset trip.
        self.consecutive_failures.store(0, Ordering::SeqCst);
    }

    /// Returns `true` if the breaker has tripped (caller should call `cooldown`).
    pub fn record_failure(&self) -> bool {
        // AcqRel: the previous value is read with Acquire semantics so we see
        // all prior stores; the new value is written with Release so concurrent
        // readers see the updated count.
        let prev = self.consecutive_failures.fetch_add(1, Ordering::AcqRel);
        let n    = prev + 1;
        if n >= self.max_failures {
            warn!(
                failures      = n,
                cooldown_secs = self.cooldown.as_secs(),
                "Circuit breaker tripped"
            );
            true
        } else {
            false
        }
    }

    pub async fn cooldown(&self) {
        tokio::time::sleep(self.cooldown).await;
        self.consecutive_failures.store(0, Ordering::SeqCst);
        info!("Circuit breaker reset");
    }
}

// ── Validation helpers ──────────────────────────────────────────────

pub fn validate_price_impact(price_impact_pct: &str, max_impact_pct: f64) -> Result<()> {
    let impact: f64 = price_impact_pct
        .parse()
        .map_err(|_| anyhow::anyhow!("Non-numeric price impact: {price_impact_pct}"))?;

    if impact.abs() > max_impact_pct {
        bail!("Price impact {impact:.2}% exceeds cap {max_impact_pct:.2}%");
    }
    Ok(())
}

pub fn validate_profitability(
    expected_profit_lamports: u64,
    min_profit_lamports:      u64,
    loan_amount_lamports:     u64,
    max_loan_lamports:        u64,
) -> Result<()> {
    if expected_profit_lamports < min_profit_lamports {
        bail!("Profit {expected_profit_lamports} < minimum {min_profit_lamports}");
    }
    if loan_amount_lamports > max_loan_lamports {
        bail!("Loan {loan_amount_lamports} > maximum {max_loan_lamports}");
    }
    Ok(())
}

// ── Deduplication ───────────────────────────────────────────────────

/// Concurrent dedup set with time-based expiry.
/// Background cleanup runs every `ttl/2` to bound memory without
/// holding a write lock on the hot path.
pub struct DeduplicatorSet {
    seen: dashmap::DashMap<String, std::time::Instant>,
    ttl:  std::time::Duration,
}

impl DeduplicatorSet {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            seen: dashmap::DashMap::new(),
            ttl:  std::time::Duration::from_secs(ttl_secs),
        }
    }

    /// Spawn a background task that periodically evicts expired entries.
    pub fn spawn_cleanup(
        self: &std::sync::Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let this     = self.clone();
        let interval = self.ttl / 2;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {
                        let now = std::time::Instant::now();
                        this.seen.retain(|_, v| now.duration_since(*v) < this.ttl);
                    }
                }
            }
        });
    }

    /// Returns `true` if `key` was already seen within the TTL window.
    ///
    /// Uses a single `entry()` call so the shard write-lock is held for the
    /// entire check-and-insert operation.  This eliminates the TOCTOU window
    /// that existed in the previous two-phase `get_mut` → `entry()` approach,
    /// where two concurrent tasks with the same brand-new key could both pass
    /// the `get_mut` check, then both call `entry().or_insert()`, and both
    /// return `false` — incorrectly treating a duplicate as new.
    ///
    /// Behaviour:
    ///   • Key present, TTL not expired → `true`  (genuine duplicate)
    ///   • Key present, TTL expired     → refresh timestamp, `false` (treat as new)
    ///   • Key absent                   → insert with current timestamp, `false`
    ///
    /// Cost: one `String` allocation per call (DashMap `entry` takes `K` by value).
    /// For Solana signatures (88 ASCII bytes) this is negligible.
    pub fn is_duplicate(&self, key: &str) -> bool {
        use dashmap::mapref::entry::Entry;

        let now = std::time::Instant::now();

        match self.seen.entry(key.to_string()) {
            Entry::Occupied(mut occ) => {
                if now.duration_since(*occ.get()) < self.ttl {
                    true  // Still fresh — genuine duplicate
                } else {
                    *occ.get_mut() = now;  // Expired — refresh in place, treat as new
                    false
                }
            }
            Entry::Vacant(v) => {
                v.insert(now);
                false
            }
        }
    }
}
