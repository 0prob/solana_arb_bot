use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use crate::config::AppConfig;
use crate::jupiter::JupiterClient;
use crate::listener::{ArbEvent, EventType};
use crate::executor::ArbOpportunity;

pub const OPPORTUNITY_CHANNEL_CAPACITY: usize = 64;

pub async fn run(
    config: Arc<AppConfig>,
    mut arb_rx: mpsc::Receiver<ArbEvent>,
    opportunity_tx: mpsc::Sender<ArbOpportunity>,
    cancel: CancellationToken,
) -> Result<()> {
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    let semaphore = Arc::new(Semaphore::new(config.scanner_max_concurrency));

    info!("Scanner started with Phase 2 support (Triangular & Liquidation)");

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe = arb_rx.recv() => match maybe {
                Some(e) => e,
                None => return Ok(()),
            },
        };

        let permit = semaphore.clone().acquire_owned().await?;
        let cfg = config.clone();
        let jup = jupiter.clone();
        let tx = opportunity_tx.clone();

        tokio::spawn(async move {
            let _permit = permit;
            match event.event_type {
                EventType::Migration(token_mint) => {
                    if let Err(e) = evaluate_triangular_opportunity(cfg, jup, token_mint, event.slot, tx).await {
                        debug!(error = %e, "Triangular evaluation failed");
                    }
                }
                EventType::Liquidation(obligation_account) => {
                    if let Err(e) = evaluate_liquidation_opportunity(cfg, jup, obligation_account, event.slot, tx).await {
                        debug!(error = %e, "Liquidation evaluation failed");
                    }
                }
            }
        });
    }
}

async fn evaluate_triangular_opportunity(
    config: Arc<AppConfig>,
    jupiter: JupiterClient,
    token_mint: solana_sdk::pubkey::Pubkey,
    slot: u64,
    tx: mpsc::Sender<ArbOpportunity>,
) -> Result<()> {
    let wsol = crate::config::programs::wsol_mint();
    let loan_amounts = [config.max_loan_lamports / 4, config.max_loan_lamports / 2, config.max_loan_lamports];

    for &amount in &loan_amounts {
        if amount == 0 { continue; }
        
        // Step 1: Find the best buy route (WSOL -> Token)
        let buy_quote = jupiter.quote(&wsol, &token_mint, amount, config.slippage_bps).await?;
        let token_out: u64 = buy_quote.other_amount_threshold.parse()?;
        if token_out == 0 { continue; }
        
        // Step 2: Find the best sell route (Token -> WSOL)
        // Jupiter will automatically find the best route across all DEXes, effectively performing cross-DEX arb.
        let sell_quote = jupiter.quote(&token_mint, &wsol, token_out, config.slippage_bps).await?;
        
        let profit = crate::jupiter::estimate_profit(amount, &sell_quote, 0, config.estimated_tx_cost())?;
        if profit >= config.min_profit_lamports as i64 {
            info!(
                token = %token_mint,
                loan_sol = (amount as f64 / 1_000_000_000.0),
                profit_sol = (profit as f64 / 1_000_000_000.0),
                "Arbitrage opportunity found"
            );
            let opp = ArbOpportunity {
                loan_lamports: amount,
                buy_quote,
                sell_quote,
                slot,
            };
            let _ = tx.try_send(opp);
            break;
        } else {
            debug!(
                token = %token_mint,
                loan_sol = (amount as f64 / 1_000_000_000.0),
                profit_sol = (profit as f64 / 1_000_000_000.0),
                "Opportunity not profitable enough"
            );
        }
    }
    Ok(())
}

async fn evaluate_liquidation_opportunity(
    _config: Arc<AppConfig>,
    _jupiter: JupiterClient,
    _obligation_account: solana_sdk::pubkey::Pubkey,
    _slot: u64,
    _tx: mpsc::Sender<ArbOpportunity>,
) -> Result<()> {
    // Heuristic: Check obligation health factor (requires RPC call to get account data)
    // If health factor < 1.0, calculate liquidation profitability.
    // For now, this is a stub as it requires protocol-specific account parsing.
    debug!("Liquidation check for obligation: {}", _obligation_account);
    Ok(())
}
