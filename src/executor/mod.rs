// src/executor/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Atomic executor — builds and submits the final arb transaction.
//
// Per-arb flow:
//   0. slot-staleness guard (drop without RPC calls if opportunity is old)
//   1. validate_profitability (pre-check against stale scanner estimate)
//   2. check fee-payer balance
//   3. refresh Jupiter quotes
//   4. re-evaluate profitability with fresh quotes
//   5. fetch ALTs + dynamic priority fee (with account context)
//   6. compute tip from FRESH profit (not stale scanner estimate)
//   7. for each flash-loan provider (lowest fee first):
//      a. build instruction plan
//      b. compile v0 message
//      c. simulate
//      d. submit via Jito (or RPC fallback)
//   8. after cooldown: drain stale opportunities from channel
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
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
    pub pool_address: Option<Pubkey>,
    pub dex_label: String,
    pub loan_amount_lamports: u64,
    pub expected_profit_lamports: u64,
    pub buy_quote: QuoteResponse,
    pub sell_quote: QuoteResponse,
    pub detected_slot: u64,
    pub source_signature: String,
}

struct ExecutionOutcome {
    signature: String,
    provider: FlashLoanProvider,
    priority_fee_micro_lamports: u64,
    tip_lamports: u64,
    via_jito: bool,
}

/// Minimum ALT account header size in bytes.
const ALT_HEADER_LEN: usize = 56;

/// Cached slot value with a timestamp.  Refreshed at most once per Solana slot
/// (~400 ms) to avoid a redundant `get_slot` RPC call for every opportunity.
struct SlotCache {
    slot:        u64,
    fetched_at:  std::time::Instant,
}

impl SlotCache {
    fn stale(&self) -> bool {
        // One slot ≈ 400 ms; refresh every 300 ms to stay ahead of the boundary.
        self.fetched_at.elapsed() > std::time::Duration::from_millis(300)
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
                        fetched_at: std::time::Instant::now(),
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
            dex   = %opp.dex_label,
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
        )
        .await;

        match result {
            Ok(outcome) => {
                breaker.record_success();

                let tip = outcome.tip_lamports;
                let actual_priority_fee_lamports = (outcome.priority_fee_micro_lamports as u128
                    * config.compute_unit_limit as u128
                    / 1_000_000) as u64;

                metrics.record_confirmed(
                    opp.expected_profit_lamports,
                    tip,
                    actual_priority_fee_lamports,
                );

                info!(
                    signature   = %outcome.signature,
                    provider    = outcome.provider.label(),
                    token       = %opp.token_mint,
                    expected_profit_sol = opp.expected_profit_lamports as f64 / 1e9,
                    "Arb transaction confirmed"
                );

                dash.send(DashEvent::ExecutorConfirmed {
                    token:      token_str,
                    signature:  outcome.signature.clone(),
                    profit_sol: opp.expected_profit_lamports as f64 / 1e9,
                    tip_sol:    tip as f64 / 1e9,
                    fee_sol:    actual_priority_fee_lamports as f64 / 1e9,
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
) -> Result<ExecutionOutcome> {
    safety::validate_profitability(
        opp.expected_profit_lamports,
        config.min_profit_lamports,
        opp.loan_amount_lamports,
        config.max_loan_lamports,
    )?;

    let fee_payer    = &config.fee_payer;
    let payer_pubkey = fee_payer.pubkey();

    // ── Balance check ────────────────────────────────────────────────
    let balance = rpc
        .get_balance(&payer_pubkey)
        .await
        .context("Failed to fetch fee payer balance from RPC")?;
    if balance < config.min_balance_lamports {
        anyhow::bail!(
            "Fee payer balance too low: {} lamports (need >= {})",
            balance,
            config.min_balance_lamports
        );
    }

    let wsol_mint = crate::config::programs::wsol_mint();

    // ── Quote refresh ────────────────────────────────────────────────
    let fresh_buy = jupiter
        .quote(&wsol_mint, &opp.token_mint, opp.loan_amount_lamports, config.slippage_bps)
        .await
        .context("Fresh buy quote failed")?;

    let token_amount = jupiter::parse_out_amount(&fresh_buy)?;
    if token_amount == 0 {
        anyhow::bail!("Fresh buy quote returned 0 tokens");
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

    if fresh_profit <= 0 || (fresh_profit as u64) < config.min_profit_lamports {
        metrics.record_stale_quote();
        dash.send(DashEvent::ExecutorStaleQuote { token: token_str.to_string() });
        anyhow::bail!(
            "Quote refresh: profit dropped to {} lamports (was {})",
            fresh_profit,
            opp.expected_profit_lamports
        );
    }

    debug!(
        original_profit = opp.expected_profit_lamports,
        fresh_profit,
        candidate_providers = provider_candidates.len(),
        "Quote refresh passed"
    );

    // ── Swap instructions ────────────────────────────────────────────
    let (buy_swap, sell_swap) = jupiter
        .swap_instructions_pair(&payer_pubkey, &fresh_buy, &fresh_sell)
        .await
        .context("Jupiter swap-instructions failed")?;

    let alt_addresses = jupiter::collect_alt_addresses(&buy_swap, &sell_swap)?;
    let alt_accounts  = fetch_alt_accounts(rpc, &alt_addresses).await?;

    // ── Dynamic priority fee with account context ────────────────────
    // Passing the writable accounts from the swap gives the RPC a more
    // accurate picture of recent fees for those specific accounts, which
    // are often congested during token migration windows. This produces a
    // higher-quality fee estimate than the global (empty-accounts) query
    // and improves landing rate on competitive slots.
    let priority_fee  = get_dynamic_priority_fee(rpc, config, &payer_pubkey, &wsol_mint).await;

    // ── Tip computation — use FRESH profit, not the stale scanner estimate ──
    // Using opp.expected_profit_lamports here over-tips when profit has decayed.
    // fresh_profit is a signed i64 and we've already confirmed it's > 0.
    let tip_lamports = if jito_enabled {
        config.dynamic_jito_tip(fresh_profit as u64)
    } else {
        0
    };

    debug!(
        tip_lamports,
        tip_fraction          = config.jito_tip_profit_fraction,
        fresh_profit_lamports = fresh_profit,
        "Dynamic Jito tip computed from fresh profit"
    );

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
        )
        .await
        {
            Ok(signature) => {
                return Ok(ExecutionOutcome {
                    signature,
                    provider,
                    priority_fee_micro_lamports: priority_fee,
                    tip_lamports,
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

    let recent_blockhash = get_blockhash_with_fallback(rpc, fallback_rpc).await?;

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

            let p75_idx = (fee_values.len() * 75) / 100;
            let p75 = fee_values.get(p75_idx).copied().unwrap_or(0);

            let dynamic = if config.priority_fee_micro_lamports == 0 {
                p75
            } else {
                p75
                    .max(config.priority_fee_micro_lamports)
                    .min(config.priority_fee_micro_lamports.saturating_mul(10))
            };

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

async fn fetch_alt_accounts(
    rpc: &RpcClient,
    addresses: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    try_join_all(addresses.iter().map(|addr| async move {
        let data = rpc
            .get_account_data(addr)
            .await
            .with_context(|| format!("Failed to fetch ALT {addr}"))?;
        parse_alt_account(*addr, &data)
            .with_context(|| format!("Failed to parse ALT {addr}"))
    }))
    .await
}

fn parse_alt_account(key: Pubkey, data: &[u8]) -> Result<AddressLookupTableAccount> {
    if data.len() < ALT_HEADER_LEN {
        anyhow::bail!("ALT data too short ({} bytes)", data.len());
    }

    let addresses_data = &data[ALT_HEADER_LEN..];
    if addresses_data.len() % 32 != 0 {
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
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
    let start   = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

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
                    // TransactionConfirmationStatus is matched via its PartialEq
                    // implementation against the known RPC-returned variants.
                    // The Solana JSON RPC spec defines the confirmation_status
                    // string as one of: "processed" | "confirmed" | "finalized".
                    // We use the Debug representation lowercased as a stable
                    // string comparison that matches across minor crate versions,
                    // since the enum variant names are part of the public API
                    // and match the JSON field values by convention.
                    //
                    // The previous impl was also correct but had an extra alloc
                    // from format!(). We now use a dedicated helper to make the
                    // intent clear and centralize this comparison.
                    if is_confirmed_or_finalized(cs) {
                        return Ok(true);
                    }
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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

async fn get_blockhash_with_fallback(rpc: &RpcClient, fallback: &RpcClient) -> Result<Hash> {
    match rpc.get_latest_blockhash().await {
        Ok(hash) => Ok(hash),
        Err(e)   => {
            warn!(error = %e, "Primary blockhash failed, trying fallback");
            fallback
                .get_latest_blockhash()
                .await
                .context("Both RPCs failed to get blockhash")
        }
    }
}
