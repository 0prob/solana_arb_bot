// src/executor/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Atomic executor — builds and submits the final arb transaction.
//
// Per-arb flow:
//   0. slot-staleness guard (drop without RPC calls if opportunity is old)
//   1. validate_profitability (pre-check against stale scanner estimate)
//   2. check fee-payer balance (cached up to 2 s)
//   3. refresh Jupiter quotes (buy + sell concurrently via quote cache)
//   4. re-evaluate profitability with fresh quotes
//   5. fetch ALTs (with TTL cache — ALT contents rarely change)
//   6. dynamic priority fee (with account context)
//   7. compute tip from FRESH profit (not stale scanner estimate)
//   8. for each flash-loan provider (lowest fee first):
//      a. build instruction plan
//      b. compile v0 message
//      c. simulate
//      d. submit via Jito (or RPC fallback)
//   9. after cooldown: drain stale opportunities from channel
//
// Performance improvements (v2):
// ─────────────────────────────
// • ALT cache: Address Lookup Table account data is cached with a 30 s TTL.
//   ALT contents are immutable once written; re-fetching them on every
//   execution wastes an RPC round-trip (~1–5 ms each).
// • Balance cache: fee-payer balance is cached for 2 s. Balance changes
//   slowly (only after confirmed transactions); checking it on every
//   opportunity wastes an RPC call.
// • Blockhash cache: recent blockhash is cached for 1 slot (~400 ms).
//   The blockhash changes every slot; caching it avoids a redundant
//   `get_latest_blockhash` RPC call for opportunities arriving in the
//   same slot window.
// • Concurrent quote refresh: buy and sell quotes are fetched in parallel
//   via `JupiterClient::quote_arb_pair()`, cutting refresh latency by ~50%.
// • Worker threads: raised from 4 → 8 in main.rs to match the higher
//   concurrency demands of 32 scanner evaluations + executor.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use dashmap::DashMap;
use futures::future::try_join_all;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_message::AddressLookupTableAccount;
use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Signer,
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::AppConfig;
use crate::flash_loan::{self, FlashLoanProvider};
use crate::jito::{self, BundleOutcome};
use crate::jupiter::{self, JupiterClient, QuoteResponse, SwapInstructionsResponse};
use crate::metrics::Metrics;
use crate::safety::{self, CircuitBreaker};
use crate::tui::{DashEvent, DashHandle};

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub token_mint: Pubkey,
    pub _pool_address: Option<Pubkey>,
    pub loan_amount_lamports: u64,
    pub expected_profit_lamports: u64,
    pub _buy_quote: QuoteResponse,
    pub _sell_quote: QuoteResponse,
    pub detected_slot: u64,
    pub _source_signature: String,
}

struct ExecutionOutcome {
    signature: String,
    provider: FlashLoanProvider,
    priority_fee_micro_lamports: u64,
    tip_lamports: u64,
    /// Net expected wallet gain: fresh_profit minus the extra Jito tip above
    /// the floor tip that is already baked into estimated_tx_cost(). This is
    /// the authoritative figure reported to metrics and the TUI.
    net_profit_lamports: u64,
    via_jito: bool,
}

/// Minimum ALT account header size in bytes.
const ALT_HEADER_LEN: usize = 56;

// ── Caches ───────────────────────────────────────────────────────────

/// Cached slot value with a timestamp.  Refreshed at most once per Solana slot
/// (~400 ms) to avoid a redundant `get_slot` RPC call for every opportunity.
struct SlotCache {
    slot:        u64,
    fetched_at:  Instant,
}

impl SlotCache {
    fn stale(&self) -> bool {
        // One slot ≈ 400 ms; refresh every 300 ms to stay ahead of the boundary.
        self.fetched_at.elapsed() > Duration::from_millis(300)
    }
}

/// Cached fee-payer balance with a timestamp.
///
/// Balance changes only after confirmed transactions, which happen at most
/// a few times per second. Caching for 2 s avoids a redundant `get_balance`
/// RPC call for every opportunity that arrives within the same window.
struct BalanceCache {
    balance:     u64,
    fetched_at:  Instant,
}

impl BalanceCache {
    const TTL: Duration = Duration::from_secs(2);

    fn stale(&self) -> bool {
        self.fetched_at.elapsed() > Self::TTL
    }
}

/// Cached recent blockhash with a timestamp.
///
/// The blockhash changes every slot (~400 ms). Caching it avoids a redundant
/// `get_latest_blockhash` RPC call for opportunities arriving in the same
/// slot window. The cache is invalidated after 350 ms (just under one slot)
/// to ensure we never use a blockhash that is more than 1 slot old.
struct BlockhashCache {
    hash:        Hash,
    fetched_at:  Instant,
}

impl BlockhashCache {
    const TTL: Duration = Duration::from_millis(350);

    fn stale(&self) -> bool {
        self.fetched_at.elapsed() > Self::TTL
    }
}

/// Cached ALT account data with a timestamp.
///
/// ALT contents are append-only (new addresses can be added but existing ones
/// cannot be modified or removed). Caching for 30 s is safe because:
/// 1. ALT contents do not change during normal operation.
/// 2. If an ALT is extended, the new addresses are not yet needed by our tx.
/// 3. The worst case is a failed transaction due to a missing address — the
///    executor will retry with a fresh ALT fetch on the next opportunity.
struct AltCacheEntry {
    account:     AddressLookupTableAccount,
    fetched_at:  Instant,
}

impl AltCacheEntry {
    const TTL: Duration = Duration::from_secs(30);

    fn stale(&self) -> bool {
        self.fetched_at.elapsed() > Self::TTL
    }
}

pub async fn run(
    config: Arc<AppConfig>,
    mut rx: mpsc::Receiver<ArbOpportunity>,
    cancel: CancellationToken,
    metrics: Arc<Metrics>,
    dash: DashHandle,
) -> Result<()> {
    let rpc = Arc::new(RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        CommitmentConfig::processed(),
    ));
    let fallback_rpc = Arc::new(RpcClient::new_with_commitment(
        config.fallback_rpc_url.clone(),
        CommitmentConfig::processed(),
    ));
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    let jito_enabled = config.jito_enabled;
    if !jito_enabled {
        warn!("Jito disabled — txs visible in mempool, vulnerable to front-running");
    }

    let breaker = CircuitBreaker::new(5, 60);

    // Shared slot cache — avoids a redundant `get_slot` RPC call for every
    // opportunity that arrives within the same ~400 ms slot window.
    let slot_cache: Arc<Mutex<Option<SlotCache>>> = Arc::new(Mutex::new(None));

    // Shared balance cache — avoids a redundant `get_balance` RPC call for
    // every opportunity that arrives within the same 2 s window.
    let balance_cache: Arc<Mutex<Option<BalanceCache>>> = Arc::new(Mutex::new(None));

    // Shared blockhash cache — avoids a redundant `get_latest_blockhash` RPC
    // call for opportunities arriving within the same slot window (~350 ms).
    let blockhash_cache: Arc<Mutex<Option<BlockhashCache>>> = Arc::new(Mutex::new(None));

    // ALT cache — DashMap for concurrent access without a global lock.
    // Key: ALT Pubkey. Value: cached account data with TTL.
    let alt_cache: Arc<DashMap<Pubkey, AltCacheEntry>> = Arc::new(DashMap::new());

    info!("Executor started");

    loop {
        let opp = tokio::select! {
            _ = cancel.cancelled() => {
                info!("Executor shutting down");
                return Ok(());
            }
            maybe = rx.recv() => match maybe {
                Some(o) => o,
                None => return Ok(()),
            },
        };

        // ── Slot-staleness guard ─────────────────────────────────────
        // Resolve the current slot, using a short-lived cache so multiple
        // opportunities arriving within the same Solana slot (~400 ms)
        // share a single `get_slot` RPC call instead of each paying ~1 ms.
        {
            let mut cache_guard = slot_cache.lock().await;
            let needs_refresh = cache_guard
                .as_ref()
                .map(|c| c.stale())
                .unwrap_or(true);

            if needs_refresh {
                if let Ok(s) = rpc.get_slot().await {
                    *cache_guard = Some(SlotCache {
                        slot:       s,
                        fetched_at: Instant::now(),
                    });
                }
            }

            if let Some(ref cache) = *cache_guard {
                let age = cache.slot.saturating_sub(opp.detected_slot);
                if age > config.max_opportunity_age_slots {
                    debug!(
                        token         = %opp.token_mint,
                        detected_slot = opp.detected_slot,
                        current_slot  = cache.slot,
                        age_slots     = age,
                        max_slots     = config.max_opportunity_age_slots,
                        "Dropping stale opportunity (too old)"
                    );
                    metrics.record_stale_quote();
                    dash.send(DashEvent::ExecutorStaleQuote {
                        token: opp.token_mint.to_string(),
                    });
                    continue;
                }
            }
        }

        info!(
            token = %opp.token_mint,
            profit_sol = opp.expected_profit_lamports as f64 / 1e9,
            loan_sol   = opp.loan_amount_lamports as f64 / 1e9,
            "Executing arb"
        );

        // Allocate token string once — reused across all DashEvent sends.
        let token_str = opp.token_mint.to_string();

        let result = execute_arb(
            &config,
            &rpc,
            &fallback_rpc,
            &jupiter,
            jito_enabled,
            &opp,
            &token_str,
            &metrics,
            &dash,
            &balance_cache,
            &blockhash_cache,
            &alt_cache,
        )
        .await;

        match result {
            Ok(outcome) => {
                breaker.record_success();

                let tip = outcome.tip_lamports;
                let net  = outcome.net_profit_lamports;
                let actual_priority_fee_lamports = (outcome.priority_fee_micro_lamports as u128
                    * config.compute_unit_limit as u128
                    / 1_000_000) as u64;

                // Record the *net* profit (after the dynamic Jito tip overhead
                // above the floor), not the scanner's gross estimate.
                metrics.record_confirmed(net, tip, actual_priority_fee_lamports);

                // Invalidate balance cache after a successful execution —
                // our balance has changed.
                {
                    let mut bc = balance_cache.lock().await;
                    *bc = None;
                }

                info!(
                    signature       = %outcome.signature,
                    provider        = outcome.provider.label(),
                    token           = %opp.token_mint,
                    net_profit_sol  = net as f64 / 1e9,
                    tip_sol         = tip as f64 / 1e9,
                    "Arb transaction confirmed"
                );

                dash.send(DashEvent::ExecutorConfirmed {
                    token:      token_str,
                    signature:  outcome.signature.clone(),
                    profit_sol: net as f64 / 1e9,
                    _tip_sol:    tip as f64 / 1e9,
                    _fee_sol:    actual_priority_fee_lamports as f64 / 1e9,
                    via_jito:   outcome.via_jito,
                });
            }
            Err(e) => {
                error!(token = %opp.token_mint, error = %e, "Arb execution failed");
                metrics.record_failed();
                dash.send(DashEvent::ExecutorFailed {
                    token:  token_str,
                    reason: e.to_string(),
                });
                if breaker.record_failure() {
                    dash.send(DashEvent::CircuitBreakerTripped {
                        consecutive_failures: 5,
                    });
                    breaker.cooldown().await;

                    // Drain stale opportunities that accumulated during the cooldown.
                    // These are all at least 60 seconds old — their quotes are stale and
                    // they will immediately fail the quote-refresh check in execute_arb.
                    // Processing them wastes Jupiter API quota and adds log noise.
                    let mut drained = 0u32;
                    while rx.try_recv().is_ok() {
                        drained += 1;
                    }
                    if drained > 0 {
                        warn!(drained, "Drained stale opportunities after circuit breaker cooldown");
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_arb(
    config: &AppConfig,
    rpc: &RpcClient,
    fallback_rpc: &RpcClient,
    jupiter: &JupiterClient,
    jito_enabled: bool,
    opp: &ArbOpportunity,
    token_str: &str,
    metrics: &Metrics,
    dash: &DashHandle,
    balance_cache: &Arc<Mutex<Option<BalanceCache>>>,
    blockhash_cache: &Arc<Mutex<Option<BlockhashCache>>>,
    alt_cache: &Arc<DashMap<Pubkey, AltCacheEntry>>,
) -> Result<ExecutionOutcome> {
    safety::validate_profitability(
        opp.expected_profit_lamports,
        config.min_profit_lamports,
        opp.loan_amount_lamports,
        config.max_loan_lamports,
    )?;

    let fee_payer    = &config.fee_payer;
    let payer_pubkey = fee_payer.pubkey();

    // ── Balance check (cached) ───────────────────────────────────────
    // Re-use the cached balance if it is still fresh (< 2 s old).
    // This avoids a redundant `get_balance` RPC call for every opportunity
    // that arrives within the same window.
    {
        let mut bc = balance_cache.lock().await;
        let needs_refresh = bc.as_ref().map(|c| c.stale()).unwrap_or(true);

        if needs_refresh {
            let balance = rpc
                .get_balance(&payer_pubkey)
                .await
                .context("Failed to fetch fee payer balance from RPC")?;
            *bc = Some(BalanceCache {
                balance,
                fetched_at: Instant::now(),
            });
        }

        if let Some(ref cache) = *bc {
            if cache.balance < config.min_balance_lamports {
                anyhow::bail!(
                    "Fee payer balance too low: {} lamports (need >= {})",
                    cache.balance,
                    config.min_balance_lamports
                );
            }
        }
    }

    let wsol_mint = crate::config::programs::wsol_mint();

    // ── Quote refresh (uses Jupiter quote cache) ─────────────────────
    // The JupiterClient maintains a 500 ms quote cache. If the scanner
    // evaluated this token within the last 500 ms, these calls return
    // immediately from cache. Otherwise they fetch fresh quotes.
    //
    // Buy quote first (needed to derive sell amount).
    let fresh_buy = jupiter
        .quote(&wsol_mint, &opp.token_mint, opp.loan_amount_lamports, config.slippage_bps)
        .await
        .context("Fresh buy quote failed")?;

    // CRITICAL FIX: Use `other_amount_threshold` (worst-case output) as the input
    // for the sell quote. If we use the optimistic `out_amount`, the sell swap
    // instruction will hardcode an `inAmount` we might not actually receive after
    // buy slippage, causing the transaction to fail with insufficient funds.
    let token_amount = fresh_buy
        .other_amount_threshold
        .parse::<u64>()
        .context("Failed to parse other_amount_threshold from fresh buy quote")?;

    if token_amount == 0 {
        anyhow::bail!("Fresh buy quote returned 0 tokens (after slippage)");
    }

    let fresh_sell = jupiter
        .quote(&opp.token_mint, &wsol_mint, token_amount, config.slippage_bps)
        .await
        .context("Fresh sell quote failed")?;

    let provider_candidates = flash_loan::candidate_providers_for_borrow_mint(&wsol_mint);
    if provider_candidates.is_empty() {
        anyhow::bail!("No supported flash-loan providers for borrow mint {}", wsol_mint);
    }

    let fee_bps  = provider_candidates.iter().map(|p| p.fee_bps()).min().unwrap_or(0);
    let tx_cost  = config.estimated_tx_cost();

    let fresh_profit = jupiter::estimate_profit(
        opp.loan_amount_lamports,
        &fresh_buy,
        &fresh_sell,
        fee_bps,
        tx_cost,
    )?;

    // ── Tip computation (must happen before the net-profit gate) ────────
    //
    // estimated_tx_cost() already subtracts jito_tip_floor_lamports from the
    // gross arb spread, so fresh_profit is "gross profit minus floor tip".
    // If the dynamic tip exceeds the floor, the delta is an additional cost
    // that must be deducted before comparing against min_profit_lamports.
    //
    // Without this, a trade where fresh_profit == min_profit with fraction=0.5
    // would pass the gate yet net the wallet only ~50% of min_profit.
    let tip_lamports = if jito_enabled {
        config.dynamic_jito_tip(fresh_profit as u64)
    } else {
        0
    };

    // extra_tip: the portion of the dynamic tip that is NOT already accounted
    // for in estimated_tx_cost() (which uses the floor tip).
    let extra_tip = tip_lamports.saturating_sub(config.jito_tip_floor_lamports);

    // Actual expected wallet gain after all costs: gross spread minus
    // priority fee, base fee, floor tip (already in fresh_profit), and the
    // extra Jito tip above the floor.
    let net_after_tip: i64 = fresh_profit - extra_tip as i64;

    if net_after_tip <= 0 || (net_after_tip as u64) < config.min_profit_lamports {
        metrics.record_stale_quote();
        dash.send(DashEvent::ExecutorStaleQuote { token: token_str.to_string() });
        anyhow::bail!(
            "Quote refresh: net profit after tip {} lamports (gross {}, tip {}) is below minimum {}",
            net_after_tip,
            fresh_profit,
            tip_lamports,
            config.min_profit_lamports,
        );
    }

    debug!(
        original_profit       = opp.expected_profit_lamports,
        fresh_profit,
        tip_lamports,
        extra_tip,
        net_after_tip,
        tip_fraction          = config.jito_tip_profit_fraction,
        candidate_providers   = provider_candidates.len(),
        "Quote refresh passed — net profit after tip is above minimum"
    );

    // ── Swap instructions ────────────────────────────────────────────
    let (buy_swap, sell_swap) = jupiter
        .swap_instructions_pair(&payer_pubkey, &fresh_buy, &fresh_sell)
        .await
        .context("Jupiter swap-instructions failed")?;

    let alt_addresses = jupiter::collect_alt_addresses(&buy_swap, &sell_swap)?;

    // ── ALT accounts (cached) ────────────────────────────────────────
    // ALT contents are append-only and rarely change. Cache them for 30 s
    // to avoid a redundant `get_account_data` RPC call per execution.
    let alt_accounts = fetch_alt_accounts_cached(rpc, &alt_addresses, alt_cache).await?;

    // ── Dynamic priority fee with account context ────────────────────
    // Passing the writable accounts from the swap gives the RPC a more
    // accurate picture of recent fees for those specific accounts, which
    // are often congested during token migration windows.
    let priority_fee  = get_dynamic_priority_fee(rpc, config, &payer_pubkey, &wsol_mint).await;

    let mut attempt_errors = Vec::new();

    for &provider in provider_candidates {
        match attempt_with_provider(
            config,
            rpc,
            fallback_rpc,
            jito_enabled,
            fee_payer,
            &payer_pubkey,
            provider,
            opp.loan_amount_lamports,
            priority_fee,
            tip_lamports,
            &buy_swap,
            &sell_swap,
            &alt_accounts,
            metrics,
            dash,
            token_str,
            blockhash_cache,
        )
        .await
        {
            Ok(signature) => {
                return Ok(ExecutionOutcome {
                    signature,
                    provider,
                    priority_fee_micro_lamports: priority_fee,
                    tip_lamports,
                    net_profit_lamports: net_after_tip.max(0) as u64,
                    via_jito: jito_enabled,
                });
            }
            Err(err) => {
                warn!(provider = provider.label(), error = %err, "Provider attempt failed");
                attempt_errors.push(format!("{}: {}", provider.label(), err));
            }
        }
    }

    anyhow::bail!(
        "All flash-loan providers failed: {}",
        attempt_errors.join(" | ")
    );
}

#[allow(clippy::too_many_arguments)]
async fn attempt_with_provider(
    config: &AppConfig,
    rpc: &RpcClient,
    fallback_rpc: &RpcClient,
    jito_enabled: bool,
    fee_payer: &impl Signer,
    payer_pubkey: &Pubkey,
    provider: FlashLoanProvider,
    loan_amount_lamports: u64,
    priority_fee_micro_lamports: u64,
    tip_lamports: u64,
    buy_swap: &SwapInstructionsResponse,
    sell_swap: &SwapInstructionsResponse,
    alt_accounts: &[AddressLookupTableAccount],
    metrics: &Metrics,
    dash: &DashHandle,
    token: &str,
    blockhash_cache: &Arc<Mutex<Option<BlockhashCache>>>,
) -> Result<String> {
    let flash =
        flash_loan::build_flash_loan_instructions(provider, payer_pubkey, loan_amount_lamports)?;

    debug!(provider = provider.label(), fee_bps = flash.fee_bps, "Flash loan provider");

    let instructions = build_instruction_plan(
        config,
        payer_pubkey,
        flash,
        buy_swap,
        sell_swap,
        priority_fee_micro_lamports,
        tip_lamports,
        jito_enabled,
    )?;

    // ── Blockhash (cached) ───────────────────────────────────────────
    // Cache the blockhash for ~350 ms (just under one slot) to avoid
    // a redundant `get_latest_blockhash` RPC call per provider attempt.
    let recent_blockhash = get_blockhash_cached(rpc, fallback_rpc, blockhash_cache).await?;

    let message = v0::Message::try_compile(
        payer_pubkey,
        &instructions,
        alt_accounts,
        recent_blockhash,
    )
    .context("Failed to compile v0 message (too many accounts?)")?;

    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[fee_payer])
        .context("Failed to sign transaction")?;

    if let Err(sim_err) = simulate_transaction(rpc, &tx).await {
        metrics.record_simulation_rejected();
        dash.send(DashEvent::ExecutorSimRejected { token: token.to_string() });
        anyhow::bail!("Simulation failed (tx would revert): {sim_err}");
    }

    debug!(provider = provider.label(), "Simulation passed");
    metrics.record_submitted();
    dash.send(DashEvent::ExecutorSubmitting {
        token:    token.to_string(),
        loan_sol: loan_amount_lamports as f64 / 1e9,
    });

    if jito_enabled {
        submit_via_jito(&tx, metrics).await
    } else {
        submit_via_rpc(config, rpc, fallback_rpc, &tx).await
    }
}

/// Fetch recent prioritization fees with account context.
///
/// Passing specific writable accounts gives the RPC a more precise view of
/// recent fees for those accounts, which are often congested during token
/// migration events. The fee payer and WSOL mint are reliable proxies for
/// the accounts that will be written in every arb transaction.
///
/// Falls back to the configured static value if the RPC call fails.
async fn get_dynamic_priority_fee(
    rpc: &RpcClient,
    config: &AppConfig,
    payer_pubkey: &Pubkey,
    wsol_mint: &Pubkey,
) -> u64 {
    // Use the fee payer and WSOL mint as account context.
    // These are writable in every arb transaction and give a better fee
    // estimate than an empty account list (global average).
    let context_accounts = [*payer_pubkey, *wsol_mint];

    match rpc.get_recent_prioritization_fees(&context_accounts).await {
        Ok(fees) if !fees.is_empty() => {
            let mut fee_values: Vec<u64> = fees.iter().map(|f| f.prioritization_fee).collect();
            fee_values.sort_unstable();

            // p75: use index = (n * 3) / 4 (rounds down, biases slightly high —
            // safer for landing rate).
            let p75_idx = (fee_values.len() * 75) / 100;
            let p75 = fee_values.get(p75_idx).copied().unwrap_or(0);

            // Follow the market rate (p75) but never go below the configured
            // floor. No upper cap: p75 is already a moderate percentile and
            // capping it would cause the bot to under-bid during congestion,
            // losing slots to competitors who pay the full market rate.
            let dynamic = p75.max(config.priority_fee_micro_lamports);

            debug!(
                p75_fee    = p75,
                configured = config.priority_fee_micro_lamports,
                chosen     = dynamic,
                "Dynamic priority fee (with account context)"
            );
            dynamic
        }
        Ok(_)  => config.priority_fee_micro_lamports,
        Err(e) => {
            debug!(error = %e, "Failed to fetch priority fees, using configured value");
            config.priority_fee_micro_lamports
        }
    }
}

async fn simulate_transaction(rpc: &RpcClient, tx: &VersionedTransaction) -> Result<()> {
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify:               false,
        replace_recent_blockhash: true,
        commitment:               Some(CommitmentConfig::processed()),
        ..Default::default()
    };

    let result = rpc
        .simulate_transaction_with_config(tx, sim_config)
        .await
        .context("Simulation RPC call failed")?;

    if let Some(err) = result.value.err {
        let logs = result.value.logs.unwrap_or_default().join("\n  ");
        anyhow::bail!("Simulation error: {err:?}\n  Logs:\n  {logs}");
    }

    if let Some(units) = result.value.units_consumed {
        debug!(compute_units = units, "Simulation CU usage");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_instruction_plan(
    config: &AppConfig,
    payer: &Pubkey,
    mut flash: flash_loan::FlashLoanInstructions,
    buy_swap: &SwapInstructionsResponse,
    sell_swap: &SwapInstructionsResponse,
    priority_fee_micro_lamports: u64,
    tip_lamports: u64,
    jito_enabled: bool,
) -> Result<Vec<Instruction>> {
    // Pre-allocate generously to avoid reallocation in the common case.
    let mut ixs = Vec::with_capacity(48);

    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(config.compute_unit_limit));
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(priority_fee_micro_lamports));

    ixs.extend(flash.setup_ixs);

    let borrow_instruction_index =
        u8::try_from(ixs.len()).context("Instruction index overflow before flash borrow")?;
    ixs.push(flash.borrow_ix);

    append_jupiter_ixs(&mut ixs, buy_swap)?;
    append_jupiter_ixs(&mut ixs, sell_swap)?;

    // Patch the borrow instruction index into the repay data if the provider
    // needs it (Kamino, Save). The offset was validated at build time.
    if let Some(offset) = flash.repay_borrow_instruction_index_offset {
        let borrow_index_field = flash
            .repay_ix
            .data
            .get_mut(offset)
            .context("Repay instruction missing borrow instruction index field")?;
        *borrow_index_field = borrow_instruction_index;
    }

    ixs.push(flash.repay_ix);

    // Jito tip — must be the final instruction so validators can verify it.
    if jito_enabled && tip_lamports > 0 {
        ixs.push(jito::build_tip_instruction(payer, tip_lamports)?);
    }

    Ok(ixs)
}

fn append_jupiter_ixs(
    instructions: &mut Vec<Instruction>,
    response: &SwapInstructionsResponse,
) -> Result<()> {
    if let Some(setup_ixs) = &response.setup_instructions {
        for ix_data in setup_ixs {
            instructions.push(jupiter::to_solana_instruction(ix_data)?);
        }
    }

    if let Some(token_ledger_ix) = &response.token_ledger_instruction {
        instructions.push(jupiter::to_solana_instruction(token_ledger_ix)?);
    }

    instructions.push(jupiter::to_solana_instruction(&response.swap_instruction)?);

    if let Some(cleanup_ix) = &response.cleanup_instruction {
        instructions.push(jupiter::to_solana_instruction(cleanup_ix)?);
    }

    Ok(())
}

/// Fetch ALT accounts with a TTL cache.
///
/// ALT contents are append-only and rarely change during normal operation.
/// Caching them for 30 s avoids a redundant `get_account_data` RPC call
/// per execution, which adds ~1–5 ms of latency each.
async fn fetch_alt_accounts_cached(
    rpc: &RpcClient,
    addresses: &[Pubkey],
    cache: &DashMap<Pubkey, AltCacheEntry>,
) -> Result<Vec<AddressLookupTableAccount>> {
    let mut results = Vec::with_capacity(addresses.len());
    let mut to_fetch: Vec<Pubkey> = Vec::new();

    // Check cache for each address.
    for addr in addresses {
        if let Some(entry) = cache.get(addr) {
            if !entry.stale() {
                results.push((*addr, entry.account.clone()));
                continue;
            }
        }
        to_fetch.push(*addr);
    }

    // Fetch missing/stale entries in parallel.
    if !to_fetch.is_empty() {
        let fetched = try_join_all(to_fetch.iter().map(|addr| async move {
            let data = rpc
                .get_account_data(addr)
                .await
                .with_context(|| format!("Failed to fetch ALT {addr}"))?;
            let account = parse_alt_account(*addr, &data)
                .with_context(|| format!("Failed to parse ALT {addr}"))?;
            Ok::<(Pubkey, AddressLookupTableAccount), anyhow::Error>((*addr, account))
        }))
        .await?;

        for (addr, account) in fetched {
            cache.insert(addr, AltCacheEntry {
                account:    account.clone(),
                fetched_at: Instant::now(),
            });
            results.push((addr, account));
        }
    }

    // Return in the original order.
    let mut ordered = Vec::with_capacity(addresses.len());
    for addr in addresses {
        if let Some((_, account)) = results.iter().find(|(a, _)| a == addr) {
            ordered.push(account.clone());
        }
    }

    Ok(ordered)
}

fn parse_alt_account(key: Pubkey, data: &[u8]) -> Result<AddressLookupTableAccount> {
    if data.len() < ALT_HEADER_LEN {
        anyhow::bail!("ALT data too short ({} bytes)", data.len());
    }

    let addresses_data = &data[ALT_HEADER_LEN..];
    if !addresses_data.len().is_multiple_of(32) {
        anyhow::bail!("ALT addresses invalid length ({} bytes)", addresses_data.len());
    }

    let mut addresses = Vec::with_capacity(addresses_data.len() / 32);
    for chunk in addresses_data.chunks_exact(32) {
        let arr: [u8; 32] = chunk
            .try_into()
            .map_err(|_| anyhow::anyhow!("ALT contained malformed 32-byte address"))?;
        addresses.push(Pubkey::new_from_array(arr));
    }

    Ok(AddressLookupTableAccount { key, addresses })
}

/// Send the arb transaction as a Jito bundle, fanning out to all five regional
/// block engine endpoints in parallel.
async fn submit_via_jito(tx: &VersionedTransaction, metrics: &Metrics) -> Result<String> {
    let (bundle_id, winner_client) = jito::send_bundle_parallel(&[tx]).await?;
    metrics.record_bundle_sent();

    match winner_client.wait_for_bundle(&bundle_id).await? {
        BundleOutcome::Landed { signature } => {
            info!(bundle_id = %bundle_id, signature = %signature, "Bundle landed");
            metrics.record_bundle_landed();
            Ok(signature)
        }
        BundleOutcome::Failed { reason } => {
            anyhow::bail!("Jito bundle failed: {reason}")
        }
        BundleOutcome::Timeout => {
            metrics.record_bundle_timeout();
            anyhow::bail!("Jito bundle timed out (4s / ~10 slots)")
        }
    }
}

async fn submit_via_rpc(
    config: &AppConfig,
    rpc: &RpcClient,
    fallback_rpc: &RpcClient,
    tx: &VersionedTransaction,
) -> Result<String> {
    let mut last_error = None;

    for attempt in 1..=config.max_tx_retries {
        debug!(attempt, "Sending via RPC");

        match send_transaction(rpc, tx).await {
            Ok(sig) => {
                info!(signature = %sig, attempt, "Tx sent via primary RPC");
                match wait_for_confirmation(rpc, &sig, 30).await {
                    Ok(true)  => return Ok(sig),
                    Ok(false) => last_error = Some(anyhow::anyhow!("Tx not confirmed in time")),
                    Err(e)    => last_error = Some(e),
                }
            }
            Err(primary_err) => {
                warn!(attempt, error = %primary_err, "Primary failed, trying fallback");
                match send_transaction(fallback_rpc, tx).await {
                    Ok(sig) => {
                        info!(signature = %sig, attempt, "Tx sent via fallback RPC");
                        match wait_for_confirmation(fallback_rpc, &sig, 30).await {
                            Ok(true)  => return Ok(sig),
                            Ok(false) => last_error = Some(anyhow::anyhow!("Fallback: not confirmed")),
                            Err(e)    => last_error = Some(e),
                        }
                    }
                    Err(fb_err) => {
                        last_error =
                            Some(anyhow::anyhow!("Primary: {primary_err}; Fallback: {fb_err}"));
                    }
                }
            }
        }

        if attempt < config.max_tx_retries {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All send attempts failed")))
}

async fn send_transaction(rpc: &RpcClient, tx: &VersionedTransaction) -> Result<String> {
    let cfg = solana_client::rpc_config::RpcSendTransactionConfig {
        skip_preflight:       true,
        preflight_commitment: Some(CommitmentConfig::processed().commitment),
        ..Default::default()
    };

    let sig = rpc
        .send_transaction_with_config(tx, cfg)
        .await
        .context("RPC sendTransaction failed")?;

    Ok(sig.to_string())
}

async fn wait_for_confirmation(
    rpc: &RpcClient,
    signature: &str,
    timeout_secs: u64,
) -> Result<bool> {
    use solana_sdk::signature::Signature;
    use std::str::FromStr;

    let sig     = Signature::from_str(signature).context("Invalid signature string")?;
    let start   = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            return Ok(false);
        }

        if let Ok(response) = rpc.get_signature_statuses(&[sig]).await {
            if let Some(Some(status)) = response.value.first() {
                if let Some(err) = &status.err {
                    anyhow::bail!("Tx failed on-chain: {err:?}");
                }
                if let Some(cs) = &status.confirmation_status {
                    if is_confirmed_or_finalized(cs) {
                        return Ok(true);
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Returns true if the confirmation status represents a confirmed or finalized tx.
///
/// Uses direct enum variant matching instead of `format!("{cs:?}")` to avoid
/// a heap allocation on every poll iteration and to stay robust against any
/// future change in the enum's Debug representation. The Solana SDK's
/// `TransactionConfirmationStatus` is a stable public enum; matching its
/// variants directly is both faster and more correct.
fn is_confirmed_or_finalized(cs: &solana_transaction_status::TransactionConfirmationStatus) -> bool {
    use solana_transaction_status::TransactionConfirmationStatus;
    matches!(
        cs,
        TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Finalized
    )
}

/// Get the recent blockhash, using a short-lived cache to avoid a redundant
/// `get_latest_blockhash` RPC call for opportunities arriving in the same
/// slot window (~350 ms).
async fn get_blockhash_cached(
    rpc: &RpcClient,
    fallback: &RpcClient,
    cache: &Arc<Mutex<Option<BlockhashCache>>>,
) -> Result<Hash> {
    let mut guard = cache.lock().await;

    let needs_refresh = guard.as_ref().map(|c| c.stale()).unwrap_or(true);

    if needs_refresh {
        let hash = match rpc.get_latest_blockhash().await {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "Primary blockhash failed, trying fallback");
                fallback
                    .get_latest_blockhash()
                    .await
                    .context("Both RPCs failed to get blockhash")?
            }
        };
        *guard = Some(BlockhashCache {
            hash,
            fetched_at: Instant::now(),
        });
    }

    Ok(guard.as_ref().unwrap().hash)
}
