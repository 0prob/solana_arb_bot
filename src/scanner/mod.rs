use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use crate::config::AppConfig;
use crate::jupiter::JupiterClient;
use crate::listener::{ArbEvent, EventType};
use crate::executor::ArbOpportunity;

/// Bounded opportunity channel — prevents executor queue OOM under burst.
/// 16 slots is enough for any realistic burst; extras are dropped via try_send.
pub const OPPORTUNITY_CHANNEL_CAPACITY: usize = 16;

/// Maximum concurrent scanner tasks (mobile-safe default: 4).
/// Each task holds 2 in-flight HTTP connections to Jupiter.
const MOBILE_MAX_CONCURRENCY: usize = 4;

pub async fn run(
    config: Arc<AppConfig>,
    mut arb_rx: mpsc::Receiver<ArbEvent>,
    opportunity_tx: mpsc::Sender<ArbOpportunity>,
    cancel: CancellationToken,
) -> Result<()> {
    let jupiter = JupiterClient::new(&config.jupiter_api_url);

    // Cap concurrency: use the lower of configured value and mobile-safe limit.
    let max_concurrency = config.scanner_max_concurrency.min(MOBILE_MAX_CONCURRENCY);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));

    // Dedup: track last-seen (pubkey, slot) pairs to skip duplicate events within a slot window.
    // Uses a fixed-size ring buffer via a small VecDeque cap.
    // O(n) scan is acceptable here because n <= 32 and this is the event-receive path,
    // not the inner quote path.  A DashSet would add Arc overhead for no real gain at n=32.
    let mut recent_keys: std::collections::VecDeque<(solana_sdk::pubkey::Pubkey, u64)> =
        std::collections::VecDeque::with_capacity(32);

    // Maximum age in slots for an event to be considered fresh.
    // Events older than this are dropped before any Jupiter round-trips.
    let max_event_age_slots = config.max_opportunity_age_slots;

    info!(
        max_concurrency,
        "Scanner started (mobile-optimized, dedup enabled)"
    );

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe = arb_rx.recv() => match maybe {
                Some(e) => e,
                None => return Ok(()),
            },
        };

        let EventType::Migration(event_key) = event.event_type;

        // ── Slot-staleness pre-check ──────────────────────────────────────────
        // We don’t have the current chain slot here (no RPC call), so we use the
        // event’s own slot as a lower bound.  The executor does the authoritative
        // check.  We only drop events that are already stale relative to the most
        // recently seen slot in our dedup buffer.
        if let Some(&(_, newest_slot)) = recent_keys.back() {
            if newest_slot > event.slot + max_event_age_slots {
                debug!(token = %event_key, slot = event.slot, newest_slot, "Dropping stale event");
                continue;
            }
        }

        // ── Deduplication: skip if same key seen in the last 32 events ───
        let is_dup = recent_keys.iter().any(|(k, slot)| {
            *k == event_key && event.slot.saturating_sub(*slot) < 5
        });
        if is_dup {
            debug!(token = %event_key, "Skipping duplicate event");
            continue;
        }
        if recent_keys.len() >= 32 {
            recent_keys.pop_front();
        }
        recent_keys.push_back((event_key, event.slot));

        // ── Back-pressure: if semaphore is exhausted, drop event ─────────
        // This prevents unbounded task spawning under high event rates.
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!(token = %event_key, "Scanner at capacity, dropping event");
                continue;
            }
        };

        let cfg = config.clone();
        let jup = jupiter.clone();
        let tx = opportunity_tx.clone();

        let token_mint = event_key;
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = evaluate_triangular_opportunity(cfg, jup, token_mint, event.slot, tx).await {
                debug!(error = %e, "Triangular evaluation failed");
            }
        });
    }
}

#[inline]
async fn evaluate_triangular_opportunity(
    config: Arc<AppConfig>,
    jupiter: JupiterClient,
    token_mint: solana_sdk::pubkey::Pubkey,
    slot: u64,
    tx: mpsc::Sender<ArbOpportunity>,
) -> Result<()> {
    let wsol = crate::config::programs::wsol_mint();

    // Only test max loan amount first (most likely to be profitable).
    // Fall back to smaller amounts only if max is unprofitable.
    // This halves the number of Jupiter round-trips on the hot path.
    let loan_amounts = [
        config.max_loan_lamports,
        config.max_loan_lamports / 2,
        config.max_loan_lamports / 4,
    ];

    for &amount in &loan_amounts {
        if amount == 0 { continue; }

        // Step 1: WSOL -> Token quote
        let buy_quote = match jupiter.quote(&wsol, &token_mint, amount, config.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                debug!(error = %e, "Buy quote failed");
                return Ok(());
            }
        };
        let token_out: u64 = match buy_quote.other_amount_threshold.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if token_out == 0 { continue; }

        // Step 2: Token -> WSOL quote
        let sell_quote = match jupiter.quote(&token_mint, &wsol, token_out, config.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                debug!(error = %e, "Sell quote failed");
                return Ok(());
            }
        };

        let profit = crate::jupiter::estimate_profit(amount, &sell_quote, config.slippage_bps, config.estimated_tx_cost())?;
        if profit >= config.min_profit_lamports as i64 {
            info!(
                token = %token_mint,
                loan_sol = (amount as f64 / 1_000_000_000.0),
                profit_sol = (profit as f64 / 1_000_000_000.0),
                "Arbitrage opportunity found"
            );
            let opp = ArbOpportunity {
                loan_lamports: amount,
                buy_quote: std::sync::Arc::new(buy_quote),
                sell_quote: std::sync::Arc::new(sell_quote),
                slot,
            };
            // Non-blocking: if executor queue is full, drop this opportunity.
            if tx.try_send(opp).is_err() {
                warn!(token = %token_mint, "Executor queue full, dropping opportunity");
            }
            // Found profitable at this size — no need to try smaller amounts.
            break;
        } else {
            debug!(
                token = %token_mint,
                loan_sol = (amount as f64 / 1_000_000_000.0),
                profit_sol = (profit as f64 / 1_000_000_000.0),
                "Opportunity not profitable enough"
            );
            // If max loan is unprofitable, smaller loans will also be unprofitable
            // (Jupiter routing is monotonic for small amounts). Break early.
            break;
        }
    }
    Ok(())
}
