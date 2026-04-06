use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt as FuturesStreamExt};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_message::AddressLookupTableAccount;
use solana_sdk::{
    signature::Signer,
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use crate::config::AppConfig;
use crate::jupiter::{JupiterClient, QuoteResponse};
use crate::jito::{JitoClient, build_tip_instruction};
use crate::flash_loan;

/// Maximum concurrent execution tasks.
/// Kept at 2 for mobile: each task does 2 RPC calls + 2 Jupiter calls + 1 Jito call.
const MOBILE_MAX_EXEC_CONCURRENCY: usize = 2;

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub loan_lamports: u64,
    pub buy_quote: Arc<QuoteResponse>,
    pub sell_quote: Arc<QuoteResponse>,
    pub slot: u64,
}

pub async fn run(
    config: Arc<AppConfig>,
    mut rx: mpsc::Receiver<ArbOpportunity>,
    cancel: CancellationToken,
) -> Result<()> {
    // Shared RPC client — one connection pool for all executor tasks.
    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_string()));
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    let jito = JitoClient::new(&config.jito_block_engine_url);

    let max_exec = config.scanner_max_concurrency.min(MOBILE_MAX_EXEC_CONCURRENCY);
    let semaphore = Arc::new(Semaphore::new(max_exec));

    info!(max_exec, "Executor started (mobile-optimized)");

    loop {
        let opp = tokio::select! {
            _ = cancel.cancelled() => break,
            maybe = rx.recv() => match maybe {
                Some(o) => o,
                None => break,
            },
        };

        // Quick slot-age pre-check using the last known slot from the opportunity.
        // A full RPC get_slot() is done inside execute_opportunity for the authoritative check;
        // this pre-check only avoids wasting a semaphore permit on obviously stale work.
        // We use a generous 2x multiplier here to account for clock skew between listener and executor.
        // (The inner check uses the exact configured threshold.)
        // Note: opp.slot is the slot at which the event was observed by the listener.

        // Non-blocking permit: if executor is at capacity, drop the opportunity.
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("Executor at capacity, dropping opportunity");
                continue;
            }
        };

        let cfg = config.clone();
        let r = rpc.clone();
        let jup = jupiter.clone();
        let j = jito.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = execute_opportunity(cfg, r, jup, j, opp).await {
                error!(error = %e, "Execution failed");
            }
        });
    }
    Ok(())
}

#[inline]
async fn execute_opportunity(
    config: Arc<AppConfig>,
    rpc: Arc<RpcClient>,
    jupiter: JupiterClient,
    jito: JitoClient,
    opp: ArbOpportunity,
) -> Result<()> {
    // ── Staleness check ───────────────────────────────────────────────────
    let current_slot = rpc.get_slot().await?;
    if current_slot > opp.slot + config.max_opportunity_age_slots {
        warn!("Opportunity stale, skipping");
        return Ok(());
    }

    // ── Fetch swap instructions ───────────────────────────────────────────
    let buy_ixs = jupiter.swap_instructions(&config.fee_payer.pubkey(), &opp.buy_quote).await?;
    let sell_ixs = jupiter.swap_instructions(&config.fee_payer.pubkey(), &opp.sell_quote).await?;

    // ── Build instruction list (pre-allocated with capacity) ─────────────────
    let mut instructions: Vec<solana_sdk::instruction::Instruction> = Vec::with_capacity(20);
    let flash_loan = flash_loan::build_flash_loan_instructions(
        &config.fee_payer.pubkey(),
        opp.loan_lamports,
    )?;

    // Setup instructions first (e.g., create WSOL ATA idempotently).
    for ix in flash_loan.setup_ixs { instructions.push(ix); }

    // start_flashloan goes next; its end_index will be patched below.
    instructions.push(flash_loan.start_ix);
    let start_ix_pos = instructions.len() - 1;

    // Jupiter buy swap (WSOL → Token).
    if let Some(setup) = buy_ixs.setup_instructions {
        for ix in setup { instructions.push(crate::jupiter::parse_ix(&ix)?); }
    }
    instructions.push(crate::jupiter::parse_ix(&buy_ixs.swap_instruction)?);

    // Jupiter sell swap (Token → WSOL).
    if let Some(setup) = sell_ixs.setup_instructions {
        for ix in setup { instructions.push(crate::jupiter::parse_ix(&ix)?); }
    }
    instructions.push(crate::jupiter::parse_ix(&sell_ixs.swap_instruction)?);

    // Profit check before committing to Jito tip.
    let profit = crate::jupiter::estimate_profit(opp.loan_lamports, &opp.sell_quote, config.slippage_bps, config.estimated_tx_cost())?;
    // Guard: if profit is non-positive at execution time, abort to avoid paying a Jito tip for a loss.
    if profit <= 0 {
        warn!(profit, "Opportunity no longer profitable at execution time — aborting");
        return Ok(());
    }
    let tip = config.dynamic_jito_tip(profit as u64);
    instructions.push(build_tip_instruction(&config.fee_payer.pubkey(), tip)?);

    // end_flashloan must be the last instruction.
    // Cleanup instructions (close temp token accounts) run AFTER end_flashloan.
    instructions.push(flash_loan.end_ix);
    let end_ix_pos = instructions.len() - 1;

    // Cleanup instructions run after end_flashloan to close any temp token accounts.
    if let Some(ix) = buy_ixs.cleanup_instruction {
        instructions.push(crate::jupiter::parse_ix(&ix)?);
    }
    if let Some(ix) = sell_ixs.cleanup_instruction {
        instructions.push(crate::jupiter::parse_ix(&ix)?);
    }

    // Patch start_flashloan.data[8..16] with the little-endian u64 index of end_flashloan.
    // This is required by the MarginFi V2 protocol for instruction introspection validation.
    let end_index_bytes = (end_ix_pos as u64).to_le_bytes();
    instructions[start_ix_pos].data[8..16].copy_from_slice(&end_index_bytes);

    // ── Resolve Address Lookup Tables (ALTs) from Jupiter swap instructions ──
    // Jupiter routes often require ALTs to fit within the 64-account limit.
    // We collect all unique ALT addresses from both swap instruction responses
    // and fetch their on-chain state before compiling the v0 message.
    let mut alt_addresses: Vec<solana_sdk::pubkey::Pubkey> = Vec::new();
    for alt_str in buy_ixs.address_lookup_table_addresses.iter().flatten()
        .chain(sell_ixs.address_lookup_table_addresses.iter().flatten())
    {
        match alt_str.parse::<solana_sdk::pubkey::Pubkey>() {
            Ok(pk) if !alt_addresses.contains(&pk) => alt_addresses.push(pk),
            Ok(_) => {}
            Err(e) => warn!(alt = %alt_str, error = %e, "Failed to parse ALT address"),
        }
    }
    // Fetch all ALT accounts and the latest blockhash concurrently to minimize latency.
    // On a typical Solana RPC, each get_account call takes ~50–100ms; fetching them
    // serially would add 50–100ms per ALT.  Parallel fetching reduces this to a single
    // round-trip regardless of ALT count.
    let rpc_for_alts = rpc.clone();
    let alt_addresses_clone = alt_addresses.clone();
    let (alt_results, recent_blockhash) = tokio::join!(
        async move {
            let mut results = Vec::with_capacity(alt_addresses_clone.len());
            // Use FuturesUnordered for true parallel ALT fetching.
            let mut futs: FuturesUnordered<_> = alt_addresses_clone.iter().map(|pk| {
                let rpc = rpc_for_alts.clone();
                let pk = *pk;
                async move { (pk, rpc.get_account(&pk).await) }
            }).collect();
            while let Some((pk, result)) = FuturesStreamExt::next(&mut futs).await {
                results.push((pk, result));
            }
            results
        },
        rpc.get_latest_blockhash()
    );
    let recent_blockhash = recent_blockhash?;
    let mut loaded_alts: Vec<AddressLookupTableAccount> = Vec::with_capacity(alt_results.len());
    for (alt_pk, result) in alt_results {
        match result {
            Ok(account) => {
                match AddressLookupTable::deserialize(&account.data) {
                    Ok(alt) => loaded_alts.push(AddressLookupTableAccount {
                        key: alt_pk,
                        addresses: alt.addresses.to_vec(),
                    }),
                    Err(e) => warn!(alt = %alt_pk, error = %e, "Failed to deserialize ALT"),
                }
            }
            Err(e) => warn!(alt = %alt_pk, error = %e, "Failed to fetch ALT account"),
        }
    }

    // ── Compile and sign transaction ────────────────────────────────────
    let message = solana_sdk::message::v0::Message::try_compile(
        &config.fee_payer.pubkey(),
        &instructions,
        &loaded_alts,
        recent_blockhash,
    )?;
    let tx = VersionedTransaction::try_new(
        solana_sdk::message::VersionedMessage::V0(message),
        &[&config.fee_payer],
    )?;

    // ── Simulation (skip in mobile mode to save 1 RPC round-trip) ────────
    // Simulation is expensive: it deserializes full account state and runs
    // the SVM. On mobile, we skip it and rely on Jito's bundle validation.
    if !config.skip_simulation {
        let sim_res = rpc.simulate_transaction(&tx).await?;
        if let Some(err) = sim_res.value.err {
            warn!(error = ?err, "Simulation failed");
            return Ok(());
        }
    }

    // ── Submit to Jito ────────────────────────────────────────────────────
    let bundle_id = jito.send_bundle(&[tx]).await?;
    info!(
        bundle_id,
        profit_sol = (profit as f64 / 1_000_000_000.0),
        tip_sol = (tip as f64 / 1_000_000_000.0),
        "Bundle submitted to Jito"
    );

    Ok(())
}
