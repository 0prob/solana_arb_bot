// src/metrics/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Runtime metrics — lightweight in-memory atomics for observability.
// No external dependencies (no Prometheus/OTEL).
// ═══════════════════════════════════════════════════════════════════════

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::tui::{DashEvent, DashHandle};

#[derive(Default)]
pub struct Metrics {
    // ── Pipeline ────────────────────────────────────────────────────
    pub opportunities_evaluated:  AtomicU64,
    pub opportunities_profitable: AtomicU64,
    pub txs_submitted:            AtomicU64,
    pub txs_confirmed:            AtomicU64,
    pub txs_failed:               AtomicU64,
    pub simulations_rejected:     AtomicU64,
    pub stale_quotes_rejected:    AtomicU64,

    // ── Financials ──────────────────────────────────────────────────
    /// Cumulative expected profit of confirmed arbs (lamports).
    pub total_expected_profit_lamports: AtomicI64,
    /// Cumulative Jito tips paid (lamports).
    pub total_tips_paid_lamports:       AtomicU64,
    /// Cumulative priority fees paid (lamports).
    pub total_priority_fees_lamports:   AtomicU64,

    // ── Jito bundle acceptance ───────────────────────────────────────
    /// Bundles sent to the block engine (one per arb attempt via Jito).
    pub bundles_sent:     AtomicU64,
    /// Bundles confirmed landed on-chain.
    pub bundles_landed:   AtomicU64,
    /// Bundles that timed out (4s / ~10 slots) without confirmation.
    pub bundles_timed_out: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_evaluated(&self) {
        self.opportunities_evaluated.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_profitable(&self) {
        self.opportunities_profitable.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_submitted(&self) {
        self.txs_submitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_confirmed(
        &self,
        expected_profit_lamports: u64,
        tip_lamports: u64,
        priority_fee_lamports: u64,
    ) {
        self.txs_confirmed.fetch_add(1, Ordering::Relaxed);
        self.total_expected_profit_lamports
            .fetch_add(expected_profit_lamports as i64, Ordering::Relaxed);
        self.total_tips_paid_lamports
            .fetch_add(tip_lamports, Ordering::Relaxed);
        self.total_priority_fees_lamports
            .fetch_add(priority_fee_lamports, Ordering::Relaxed);
    }

    pub fn record_failed(&self) {
        self.txs_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_simulation_rejected(&self) {
        self.simulations_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stale_quote(&self) {
        self.stale_quotes_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bundle_sent(&self) {
        self.bundles_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bundle_landed(&self) {
        self.bundles_landed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bundle_timeout(&self) {
        self.bundles_timed_out.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            evaluated:      self.opportunities_evaluated.load(Ordering::Relaxed),
            profitable:     self.opportunities_profitable.load(Ordering::Relaxed),
            submitted:      self.txs_submitted.load(Ordering::Relaxed),
            confirmed:      self.txs_confirmed.load(Ordering::Relaxed),
            failed:         self.txs_failed.load(Ordering::Relaxed),
            sim_rejected:   self.simulations_rejected.load(Ordering::Relaxed),
            stale_rejected: self.stale_quotes_rejected.load(Ordering::Relaxed),
            expected_profit_sol: self.total_expected_profit_lamports.load(Ordering::Relaxed) as f64 / 1e9,
            _tips_sol:        self.total_tips_paid_lamports.load(Ordering::Relaxed) as f64 / 1e9,
            _priority_fees_sol: self.total_priority_fees_lamports.load(Ordering::Relaxed) as f64 / 1e9,
            bundles_sent:    self.bundles_sent.load(Ordering::Relaxed),
            bundles_landed:  self.bundles_landed.load(Ordering::Relaxed),
            bundles_timed_out: self.bundles_timed_out.load(Ordering::Relaxed),
        }
    }
}

struct MetricsSnapshot {
    evaluated: u64,
    profitable: u64,
    submitted: u64,
    confirmed: u64,
    failed: u64,
    sim_rejected: u64,
    stale_rejected: u64,
    expected_profit_sol: f64,
    _tips_sol: f64,
    _priority_fees_sol: f64,
    bundles_sent: u64,
    bundles_landed: u64,
    bundles_timed_out: u64,
}

/// Spawn a background task that pushes a metrics snapshot to the TUI every
/// `interval_secs` seconds.
pub fn spawn_reporter(
    metrics: Arc<Metrics>,
    interval_secs: u64,
    cancel: CancellationToken,
    dash: DashHandle,
) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {},
            }

            let s = metrics.snapshot();

            // expected_profit_sol is already net of tx costs — estimate_profit()
            // subtracts estimated_tx_cost() before storing. Do NOT subtract
            // tips/fees again.
            let net_profit_sol = s.expected_profit_sol;

            dash.send(DashEvent::MetricsTick {
                evaluated:        s.evaluated,
                profitable:       s.profitable,
                submitted:        s.submitted,
                confirmed:        s.confirmed,
                failed:           s.failed,
                sim_rejected:     s.sim_rejected,
                stale_rejected:   s.stale_rejected,
                net_profit_sol,
                bundles_sent:     s.bundles_sent,
                bundles_landed:   s.bundles_landed,
                bundles_timed_out: s.bundles_timed_out,
            });
        }
    });
}
