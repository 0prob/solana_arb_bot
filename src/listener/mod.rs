use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    SubscribeRequest, SubscribeRequestFilterTransactions, SubscribeRequestFilterAccounts,
};
use futures::StreamExt;
use crate::config::AppConfig;

pub const MIGRATION_CHANNEL_CAPACITY: usize = 512;

#[derive(Debug, Clone)]
pub enum EventType {
    Migration(Pubkey),
    Liquidation(Pubkey), // Pubkey of the obligation account
}

#[derive(Debug, Clone)]
pub struct ArbEvent {
    pub event_type: EventType,
    pub slot: u64,
}

pub async fn run(
    config: Arc<AppConfig>,
    tx: mpsc::Sender<ArbEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut client = GeyserGrpcClient::build_from_shared(config.grpc_endpoint.clone())?
        .x_token(Some(&config.grpc_x_token))?
        .connect()
        .await?;

    let mut transactions = std::collections::HashMap::new();
    transactions.insert(
        "migration".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![],
        },
    );

    let mut accounts = std::collections::HashMap::new();
    let dex_map = crate::dex_registry::detectable_dex_map();
    for entry in dex_map.iter() {
        let program_id = entry.key();
        accounts.insert(
            format!("dex_{}", program_id),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![program_id.to_string()],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );
    }

    // Subscribe to lending protocols for liquidations
    let lending_programs = crate::config::programs::lending_programs();
    for program_id in lending_programs {
        accounts.insert(
            format!("lending_{}", program_id),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![program_id.to_string()],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );
    }

    let request = SubscribeRequest {
        transactions,
        accounts,
        ..Default::default()
    };

    let (_, mut stream) = client.subscribe_with_request(Some(request)).await?;
    info!("Listener connected to gRPC with transaction, account, and lending filters");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            update = stream.next() => {
                let update = match update {
                    Some(Ok(u)) => u,
                    Some(Err(e)) => {
                        warn!(error = %e, "gRPC stream error");
                        continue;
                    }
                    None => break,
                };

                match update.update_oneof {
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Transaction(tx_update)) => {
                        process_transaction(&tx_update, &tx, dex_map).await?;
                    }
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Account(acc_update)) => {
                        process_account_update(&acc_update, &tx, dex_map).await?;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn process_transaction(
    tx_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    tx: &mpsc::Sender<ArbEvent>,
    dex_map: &dashmap::DashMap<Pubkey, &'static str>,
) -> Result<()> {
    let transaction = tx_update.transaction.as_ref().context("Missing transaction")?;
    let message = transaction.transaction.as_ref().and_then(|t| t.message.as_ref()).context("Missing message")?;
    for ix in &message.instructions {
        let program_id_idx = ix.program_id_index as usize;
        let program_id_bytes = message.account_keys.get(program_id_idx).context("Invalid program index")?;
        let program_id = Pubkey::try_from(program_id_bytes.as_slice()).map_err(|_| anyhow::anyhow!("Invalid program ID"))?;
        
        if dex_map.contains_key(&program_id) {
            let wsol = crate::config::programs::wsol_mint();
            for &idx in &ix.accounts {
                let pk_bytes = message.account_keys.get(idx as usize).context("Invalid account index")?;
                let pk = Pubkey::try_from(pk_bytes.as_slice()).map_err(|_| anyhow::anyhow!("Invalid account ID"))?;
                if pk != program_id && pk != wsol {
                    debug!(token = %pk, "Pool detected via transaction");
                    let _ = tx.try_send(ArbEvent {
                        event_type: EventType::Migration(pk),
                        slot: tx_update.slot,
                    });
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn process_account_update(
    acc_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateAccount,
    tx: &mpsc::Sender<ArbEvent>,
    dex_map: &dashmap::DashMap<Pubkey, &'static str>,
) -> Result<()> {
    if let Some(account) = &acc_update.account {
        let pk = Pubkey::try_from(account.pubkey.as_slice()).context("Invalid pubkey")?;
        let owner = Pubkey::try_from(account.owner.as_slice()).context("Invalid owner")?;
        
        if dex_map.contains_key(&owner) {
            debug!(account = %pk, "DEX account update detected, triggering scan");
            let _ = tx.try_send(ArbEvent {
                event_type: EventType::Migration(pk),
                slot: acc_update.slot,
            });
        } else if crate::config::programs::lending_programs().contains(&owner) {
            debug!(account = %pk, "Lending account update detected, triggering liquidation check");
            let _ = tx.try_send(ArbEvent {
                event_type: EventType::Liquidation(pk),
                slot: acc_update.slot,
            });
        }
    }
    Ok(())
}
