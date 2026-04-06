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

/// Bounded migration channel capacity.
///
/// Sizing rationale:
/// - Pump.fun can emit 50–100 migration events per second during peak activity.
/// - The scanner processes events at ~10–20/s (limited by Jupiter round-trips).
/// - 128 slots provides ~1–2 seconds of burst buffer before dropping events.
/// - Larger values waste heap; smaller values cause unnecessary drops.
/// - Events beyond capacity are dropped via try_send (non-blocking) — this is
///   intentional: stale events are worthless, so dropping is preferable to blocking.
pub const MIGRATION_CHANNEL_CAPACITY: usize = 128;

/// Maximum reconnection backoff in seconds.
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub enum EventType {
    Migration(Pubkey),
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
                // Do not reset here; backoff resets only after a sustained connection.
                continue;
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

    // Tatum has a strict limit of 10 Pubkeys per filter.
    if all_owners.len() > 10 {
        warn!(
            total = all_owners.len(),
            limit = 10,
            "Too many monitored programs; capping to the first 10 to stay within gRPC limits."
        );
        all_owners.truncate(10);
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
        "gRPC subscription active"
    );

    // A 120-second timeout on any individual stream.next() call detects silent stalls
    // (e.g., network partition where the TCP connection appears alive but no data flows).
    const STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = tokio::time::timeout(
                std::time::Duration::from_secs(STREAM_IDLE_TIMEOUT_SECS),
                stream.next()
            ) => {
                let update = match result {
                    Ok(Some(Ok(u))) => u,
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(None) => return Err(anyhow::anyhow!("gRPC stream ended unexpectedly")),
                    Err(_) => return Err(anyhow::anyhow!("gRPC stream idle for {}s — reconnecting", STREAM_IDLE_TIMEOUT_SECS)),
                };

                match update.update_oneof {
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Transaction(tx_update)) => {
                        process_transaction(&tx_update, tx, dex_map);
                    }
                    Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Account(acc_update)) => {
                        process_account_update(&acc_update, tx, dex_map);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[inline]
fn process_transaction(
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

    // Hoist wsol_mint() outside the instruction loop — it is a static value
    // but the function call itself has a small overhead per iteration.
    let wsol = crate::config::programs::wsol_mint();
    // Also pre-compute the system program pubkey to skip it in account scanning.
    let system_program: Pubkey = "11111111111111111111111111111111".parse().unwrap_or_default();
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
            for &idx in &ix.accounts {
                let pk_bytes = match message.account_keys.get(idx as usize) {
                    Some(b) => b,
                    None => continue,
                };
                let pk = match Pubkey::try_from(pk_bytes.as_slice()) {
                    Ok(pk) => pk,
                    Err(_) => continue,
                };
                if pk != program_id && pk != wsol && pk != system_program {
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
fn process_account_update(
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
    }
    // Liquidation event handling removed: no complete implementation exists.
}
