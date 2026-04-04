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

/// Bounded migration channel — 64 slots prevents OOM under burst.
/// Events beyond capacity are dropped via try_send (non-blocking).
pub const MIGRATION_CHANNEL_CAPACITY: usize = 64;

/// Maximum reconnection backoff in seconds.
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub enum EventType {
    Migration(Pubkey),
    Liquidation(Pubkey),
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
    let mut backoff_secs: u64 = 5;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        match run_inner(&config, &tx, &cancel).await {
            Ok(()) => {
                // Graceful shutdown via cancellation token.
                return Ok(());
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                warn!(
                    error = %e,
                    backoff_secs,
                    "gRPC listener error — reconnecting with backoff"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
        }
    }
}

async fn run_inner(
    config: &Arc<AppConfig>,
    tx: &mpsc::Sender<ArbEvent>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut client = GeyserGrpcClient::build_from_shared(config.grpc_endpoint.to_string())?
        .x_token(Some(config.grpc_x_token.as_ref()))?
        .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())?
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .connect()
        .await
        .context("gRPC connect")?;

    // ── Build subscription filters ────────────────────────────────────────
    // Subscribe to transactions only for known DEX programs (not all txns).
    // This dramatically reduces gRPC stream volume on mobile.
    let mut transactions = std::collections::HashMap::new();
    transactions.insert(
        "arb".to_string(),
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
    
    // Consolidate all DEX and lending programs into a single filter to stay under the 10-filter limit.
    let mut all_owners = Vec::new();
    for entry in dex_map.iter() {
        all_owners.push(entry.key().to_string());
    }

    let lending_programs = crate::config::programs::lending_programs();
    for program_id in &lending_programs {
        all_owners.push(program_id.to_string());
    }

    if !all_owners.is_empty() {
        accounts.insert(
            "monitored_programs".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: all_owners,
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

    let (_, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .context("gRPC subscribe")?;

    info!(
        endpoint = %config.grpc_endpoint,
        dex_count = dex_map.len(),
        lending_count = lending_programs.len(),
        "gRPC subscription active"
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            update = stream.next() => {
                let update = match update {
                    Some(Ok(u)) => u,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("gRPC stream ended")),
                };

                match update.update_oneof {
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Transaction(tx_update)) => {
                        process_transaction(&tx_update, tx, dex_map).await;
                    }
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Account(acc_update)) => {
                        process_account_update(&acc_update, tx, dex_map).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[inline]
async fn process_transaction(
    tx_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    tx: &mpsc::Sender<ArbEvent>,
    dex_map: &dashmap::DashMap<Pubkey, &'static str>,
) {
    let transaction = match tx_update.transaction.as_ref() {
        Some(t) => t,
        None => return,
    };
    let message = match transaction.transaction.as_ref().and_then(|t| t.message.as_ref()) {
        Some(m) => m,
        None => return,
    };

    for ix in &message.instructions {
        let program_id_idx = ix.program_id_index as usize;
        let program_id_bytes = match message.account_keys.get(program_id_idx) {
            Some(b) => b,
            None => continue,
        };
        let program_id = match Pubkey::try_from(program_id_bytes.as_slice()) {
            Ok(pk) => pk,
            Err(_) => continue,
        };

        if dex_map.contains_key(&program_id) {
            let wsol = crate::config::programs::wsol_mint();
            for &idx in &ix.accounts {
                let pk_bytes = match message.account_keys.get(idx as usize) {
                    Some(b) => b,
                    None => continue,
                };
                let pk = match Pubkey::try_from(pk_bytes.as_slice()) {
                    Ok(pk) => pk,
                    Err(_) => continue,
                };
                if pk != program_id && pk != wsol {
                    debug!(token = %pk, "Pool detected via transaction");
                    let _ = tx.try_send(ArbEvent {
                        event_type: EventType::Migration(pk),
                        slot: tx_update.slot,
                    });
                    return; // Only emit one event per transaction
                }
            }
        }
    }
}

#[inline]
async fn process_account_update(
    acc_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateAccount,
    tx: &mpsc::Sender<ArbEvent>,
    dex_map: &dashmap::DashMap<Pubkey, &'static str>,
) {
    let account = match &acc_update.account {
        Some(a) => a,
        None => return,
    };
    let pk = match Pubkey::try_from(account.pubkey.as_slice()) {
        Ok(pk) => pk,
        Err(_) => return,
    };
    let owner = match Pubkey::try_from(account.owner.as_slice()) {
        Ok(pk) => pk,
        Err(_) => return,
    };

    if dex_map.contains_key(&owner) {
        debug!(account = %pk, "DEX account update detected");
        let _ = tx.try_send(ArbEvent {
            event_type: EventType::Migration(pk),
            slot: acc_update.slot,
        });
    } else if crate::config::programs::lending_programs().contains(&owner) {
        debug!(account = %pk, "Lending account update detected");
        let _ = tx.try_send(ArbEvent {
            event_type: EventType::Liquidation(pk),
            slot: acc_update.slot,
        });
    }
}
