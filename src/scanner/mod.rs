use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use crate::config::AppConfig;
use crate::jupiter::JupiterClient;
use crate::listener::MigrationEvent;
use crate::executor::ArbOpportunity;

pub const OPPORTUNITY_CHANNEL_CAPACITY: usize = 64;

pub async fn run(
    config: Arc<AppConfig>,
    mut migration_rx: mpsc::Receiver<MigrationEvent>,
    opportunity_tx: mpsc::Sender<ArbOpportunity>,
    cancel: CancellationToken,
) -> Result<()> {
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    let semaphore = Arc::new(Semaphore::new(config.scanner_max_concurrency));

    info!("Scanner started");

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe = migration_rx.recv() => match maybe {
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
            if let Err(e) = evaluate_opportunity(cfg, jup, event, tx).await {
                debug!(error = %e, "Evaluation failed");
            }
        });
    }
}

async fn evaluate_opportunity(
    config: Arc<AppConfig>,
    jupiter: JupiterClient,
    event: MigrationEvent,
    tx: mpsc::Sender<ArbOpportunity>,
) -> Result<()> {
    let wsol = crate::config::programs::wsol_mint();
    let loan_amounts = [config.max_loan_lamports / 4, config.max_loan_lamports / 2, config.max_loan_lamports];

    for &amount in &loan_amounts {
        if amount == 0 { continue; }
        let buy_quote = jupiter.quote(&wsol, &event.token_mint, amount, config.slippage_bps).await?;
        let token_out: u64 = buy_quote.other_amount_threshold.parse()?;
        if token_out == 0 { continue; }
        let sell_quote = jupiter.quote(&event.token_mint, &wsol, token_out, config.slippage_bps).await?;
        
        let profit = crate::jupiter::estimate_profit(amount, &sell_quote, 0, config.estimated_tx_cost())?;
        if profit >= config.min_profit_lamports as i64 {
            let opp = ArbOpportunity {
                loan_lamports: amount,
                buy_quote,
                sell_quote,
                slot: event.slot,
            };
            let _ = tx.try_send(opp);
            break;
        }
    }
    Ok(())
}
