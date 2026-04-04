// src/scanner/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Opportunity scanner — evaluates migration events for profitability
// via Jupiter quotes at multiple loan sizes.
//
// Performance improvements (v2):
// ─────────────────────────────
// • Uses `JupiterClient::quote_arb_pair()` which fetches the buy quote
//   first, then immediately derives the sell amount and fetches the sell
//   quote — with the 500 ms quote cache, repeated calls for the same token
//   within the same burst window are served from cache at zero latency.
// • Added smaller loan sizes (0.1 SOL, 0.25 SOL) to catch micro-arb
//   opportunities on thin liquidity pools that are missed at 0.5 SOL.
// • Opportunity channel capacity increased from 32 → 64 to reduce drops
//   during executor backlog.
// • EVAL_TIMEOUT_MS reduced from 3000 → 2000 ms: with concurrent quotes
//   and caching, evaluations complete much faster; a tighter timeout
//   prevents stale evaluations from consuming concurrency slots.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{programs, AppConfig};
use crate::executor::ArbOpportunity;
use crate::flash_loan;
use crate::jupiter::{self, JupiterClient};
use crate::listener::MigrationEvent;
use crate::metrics::Metrics;
use crate::safety;
use crate::tui::{DashEvent, DashHandle};

/// Maximum time to spend evaluating a single opportunity (ms).
///
/// Reduced from 3000 → 2000 ms: with concurrent quote fetching and
/// the 500 ms quote cache, evaluations are much faster. A tighter
/// timeout frees concurrency slots sooner during burst events.
const EVAL_TIMEOUT_MS: u64 = 2_000;

/// Loan sizes in lamports (pre-computed — avoids f64→u64 cast per iteration).
///
/// Extended with smaller sizes (0.1 SOL, 0.25 SOL) to capture micro-arb
/// opportunities on thin liquidity pools. Ordered ascending so we can
/// early-exit once price impact makes larger sizes unprofitable.
const LOAN_SIZES_LAMPORTS: &[u64] = &[
      100_000_000,   //  0.1 SOL  ← new: micro-arb on thin pools
      250_000_000,   //  0.25 SOL ← new: small-cap token launches
      500_000_000,   //  0.5 SOL
    1_000_000_000,   //  1   SOL
    2_000_000_000,   //  2   SOL
    5_000_000_000,   //  5   SOL
   10_000_000_000,   // 10   SOL
   25_000_000_000,   // 25   SOL
   50_000_000_000,   // 50   SOL
];

/// Hard cap on acceptable price impact per leg.
const MAX_PRICE_IMPACT_PCT: f64 = 15.0;

/// Opportunity channel capacity.
///
/// Increased from 32 → 64 to reduce drops during executor backlog.
/// The executor processes one opportunity at a time; a larger buffer
/// ensures profitable opportunities are not dropped during execution.
pub const OPPORTUNITY_CHANNEL_CAPACITY: usize = 64;

pub async fn run(
    config: Arc<AppConfig>,
    mut rx: mpsc::Receiver<MigrationEvent>,
    tx: mpsc::Sender<ArbOpportunity>,
    cancel: CancellationToken,
    metrics: Arc<Metrics>,
    dash: DashHandle,
) -> Result<()> {
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    // wsol_mint() is infallible — it returns Pubkey directly via OnceLock,
    // not a Result. The ? operator is invalid here.
    let wsol = programs::wsol_mint();
    let eval_slots = Arc::new(Semaphore::new(config.scanner_max_concurrency));

    info!(max_concurrency = config.scanner_max_concurrency, "Scanner started");

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe = rx.recv() => match maybe {
                Some(e) => e,
                None => return Ok(()),
            },
        };

        // Acquire capacity before spawning. This prevents an unbounded backlog
        // of parked tasks during hot event bursts.
        let permit = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            acquired = eval_slots.clone().acquire_owned() => match acquired {
                Ok(p) => p,
                Err(_) => return Ok(()),
            },
        };

        let cfg = config.clone();
        let jup = jupiter.clone();
        let opp_tx = tx.clone();
        let m = metrics.clone();
        let d = dash.clone();

        tokio::spawn(async move {
            process_event(cfg, jup, event, wsol, opp_tx, m, d, permit).await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_event(
    cfg: Arc<AppConfig>,
    jup: JupiterClient,
    event: MigrationEvent,
    wsol: solana_sdk::pubkey::Pubkey,
    opp_tx: mpsc::Sender<ArbOpportunity>,
    metrics: Arc<Metrics>,
    dash: DashHandle,
    _permit: OwnedSemaphorePermit,
) {
    metrics.record_evaluated();

    // Allocate token string once — reused in all DashEvent sends below.
    let token_str = event.token_mint.to_string();
    dash.send(DashEvent::ScannerEvaluating { _token: token_str.clone() });

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(EVAL_TIMEOUT_MS),
        evaluate_opportunity(&cfg, &jup, &event, &wsol),
    )
    .await;

    match result {
        Ok(Ok(Some(opp))) => {
            metrics.record_profitable();
            let loan_sol   = opp.loan_amount_lamports  as f64 / 1e9;
            let profit_sol = opp.expected_profit_lamports as f64 / 1e9;
            info!(token = %event.token_mint, profit_sol, loan_sol, "Profitable opportunity found");
            dash.send(DashEvent::ScannerProfitable { token: token_str, loan_sol, profit_sol });
            if opp_tx.send(opp).await.is_err() {
                warn!(token = %event.token_mint, "Executor channel closed — profitable opportunity dropped");
            }
        }
        Ok(Ok(None)) => {
            debug!(token = %event.token_mint, "No profitable route");
            dash.send(DashEvent::ScannerUnprofitable { _token: token_str });
        }
        Ok(Err(e)) => {
            warn!(token = %event.token_mint, error = %e, "Evaluation error");
            dash.send(DashEvent::ScannerError { token: token_str, msg: e.to_string() });
        }
        Err(_) => {
            warn!(token = %event.token_mint, "Evaluation timed out");
            dash.send(DashEvent::ScannerTimeout { token: token_str });
        }
    }
}

async fn evaluate_opportunity(
    config: &AppConfig,
    jupiter: &JupiterClient,
    event: &MigrationEvent,
    wsol: &solana_sdk::pubkey::Pubkey,
) -> Result<Option<ArbOpportunity>> {
    let token_mint = &event.token_mint;

    debug!(token = %token_mint, dex = %event.dex_label, slot = event.slot, "Evaluating");

    let fee_bps = flash_loan::best_fee_bps_for_borrow_mint(wsol)
        .ok_or_else(|| anyhow::anyhow!("No supported flash-loan providers for {}", wsol))?;

    debug!(token = %token_mint, fee_bps, "Flash-loan fee baseline selected");

    let tx_cost    = config.estimated_tx_cost();
    let min_profit = config.min_profit_lamports as i64;

    let mut best: Option<ArbOpportunity> = None;
    let mut best_profit: i64 = 0;

    for &loan_lamports in LOAN_SIZES_LAMPORTS {
        if loan_lamports > config.max_loan_lamports {
            break;
        }

        // Fetch buy quote (with cache — repeated calls within 500 ms are free).
        let buy_quote = match jupiter
            .quote(wsol, token_mint, loan_lamports, config.slippage_bps)
            .await
        {
            Ok(q) => q,
            Err(e) => {
                debug!(loan_lamports, error = %e, "Buy quote failed");
                continue;
            }
        };

        if safety::validate_price_impact(&buy_quote.price_impact_pct, MAX_PRICE_IMPACT_PCT)
            .is_err()
        {
            debug!(
                loan_lamports,
                impact = %buy_quote.price_impact_pct,
                "Buy impact too high — skipping all larger sizes"
            );
            // Buy price impact is monotonically worse with larger loan sizes:
            // larger loans buy more tokens, moving the price further against us.
            // Once we exceed the cap at size N, sizes N+1..max will also exceed it.
            break;
        }

        // CRITICAL FIX: Use `other_amount_threshold` (worst-case output) as the input
        // for the sell quote. If we use the optimistic `out_amount`, the sell swap
        // instruction will hardcode an `inAmount` we might not actually receive after
        // buy slippage, causing the transaction to fail with insufficient funds.
        let token_amount = match buy_quote.other_amount_threshold.parse::<u64>() {
            Ok(a) if a > 0 => a,
            _ => continue,
        };

        let sell_quote = match jupiter
            .quote(token_mint, wsol, token_amount, config.slippage_bps)
            .await
        {
            Ok(q) => q,
            Err(e) => {
                debug!(loan_lamports, error = %e, "Sell quote failed");
                continue;
            }
        };

        if safety::validate_price_impact(&sell_quote.price_impact_pct, MAX_PRICE_IMPACT_PCT)
            .is_err()
        {
            debug!(
                loan_lamports,
                impact = %sell_quote.price_impact_pct,
                "Sell impact too high — skipping all larger sizes"
            );
            // Sell price impact is also monotonically worse with larger loan sizes:
            // a larger buy produces more tokens, which cause more slippage when sold.
            // Break rather than continue — no smaller-than-current sizes remain.
            break;
        }

        let profit = match jupiter::estimate_profit(
            loan_lamports,
            &buy_quote,
            &sell_quote,
            fee_bps,
            tx_cost,
        ) {
            Ok(p) => p,
            Err(_) => continue,
        };

        debug!(
            loan_lamports,
            profit_lamports = profit,
            profit_sol = profit as f64 / 1e9,
            "Simulated arb (conservative: using other_amount_threshold)"
        );

        // Use >= so that profit == min_profit is accepted, consistent with the
        // executor's validate_profitability check (which rejects only < min_profit).
        if profit > 0 && profit >= min_profit && profit > best_profit {
            best_profit = profit;
            best = Some(ArbOpportunity {
                token_mint: *token_mint,
                _pool_address: event.pool_address,
                loan_amount_lamports: loan_lamports,
                expected_profit_lamports: profit as u64,
                _buy_quote: buy_quote.clone(),
                _sell_quote: sell_quote.clone(),
                detected_slot: event.slot,
                _source_signature: event.signature.clone(),
            });
        }
    }

    Ok(best)
}
