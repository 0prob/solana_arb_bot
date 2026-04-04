use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
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

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub loan_lamports: u64,
    pub buy_quote: QuoteResponse,
    pub sell_quote: QuoteResponse,
    pub slot: u64,
}

pub async fn run(
    config: Arc<AppConfig>,
    mut rx: mpsc::Receiver<ArbOpportunity>,
    cancel: CancellationToken,
) -> Result<()> {
    let rpc = Arc::new(RpcClient::new(config.rpc_url.clone()));
    let jupiter = JupiterClient::new(&config.jupiter_api_url);
    let jito = JitoClient::new(&config.jito_block_engine_url);
    let semaphore = Arc::new(Semaphore::new(config.scanner_max_concurrency)); // Reuse scanner concurrency for executor
    
    info!("Executor started");

    loop {
        let opp = tokio::select! {
            _ = cancel.cancelled() => break,
            maybe = rx.recv() => match maybe {
                Some(o) => o,
                None => break,
            },
        };

        let permit = semaphore.clone().acquire_owned().await?;
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

async fn execute_opportunity(
    config: Arc<AppConfig>,
    rpc: Arc<RpcClient>,
    jupiter: JupiterClient,
    jito: JitoClient,
    opp: ArbOpportunity,
) -> Result<()> {
    let current_slot = rpc.get_slot().await?;
    if current_slot > opp.slot + config.max_opportunity_age_slots {
        warn!("Opportunity stale");
        return Ok(());
    }

    let buy_ixs = jupiter.swap_instructions(&config.fee_payer.pubkey(), &opp.buy_quote).await?;
    let sell_ixs = jupiter.swap_instructions(&config.fee_payer.pubkey(), &opp.sell_quote).await?;

    let mut instructions = Vec::new();
    let flash_loan = flash_loan::build_flash_loan_instructions(
        &config.fee_payer.pubkey(),
        opp.loan_lamports,
    )?;

    for ix in flash_loan.setup_ixs { instructions.push(ix); }
    instructions.push(flash_loan.borrow_ix);
    if let Some(setup) = buy_ixs.setup_instructions {
        for ix in setup { instructions.push(crate::jupiter::parse_ix(&ix)?); }
    }
    instructions.push(crate::jupiter::parse_ix(&buy_ixs.swap_instruction)?);
    if let Some(setup) = sell_ixs.setup_instructions {
        for ix in setup { instructions.push(crate::jupiter::parse_ix(&ix)?); }
    }
    instructions.push(crate::jupiter::parse_ix(&sell_ixs.swap_instruction)?);
    
    let profit = crate::jupiter::estimate_profit(opp.loan_lamports, &opp.sell_quote, 0, config.estimated_tx_cost())?;
    let tip = config.dynamic_jito_tip(profit as u64);
    instructions.push(build_tip_instruction(&config.fee_payer.pubkey(), tip)?);
    instructions.push(flash_loan.repay_ix);

    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let message = solana_sdk::message::v0::Message::try_compile(
        &config.fee_payer.pubkey(),
        &instructions,
        &[],
        recent_blockhash,
    )?;
    let tx = VersionedTransaction::try_new(solana_sdk::message::VersionedMessage::V0(message), &[&config.fee_payer])?;

    // Simulation before sending
    let sim_res = rpc.simulate_transaction(&tx).await?;
    if let Some(err) = sim_res.value.err {
        warn!(error = ?err, logs = ?sim_res.value.logs, "Simulation failed");
        return Ok(());
    }

    let bundle_id = jito.send_bundle(&[tx]).await?;
    info!(bundle_id, "Bundle submitted");

    Ok(())
}
