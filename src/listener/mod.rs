// src/listener/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Real-time gRPC listener — subscribes to all detectable DEX programs
// via Yellowstone gRPC and emits MigrationEvent for pool creations.
//
// Transaction parsing pipeline
// ────────────────────────────
// 1. Receive SubscribeUpdateTransaction from the gRPC stream.
// 2. Reject votes, failed transactions, and previously-seen signatures.
// 3. Build a flat AccountTable (static keys + ALT-loaded keys from meta).
// 4. Walk ALL instructions — both outer (top-level) and inner (CPI) —
//    looking for any that target a detectable DEX program.
//    Inner instructions are critical: launchers and bundlers frequently
//    invoke pool creation via CPI, not as a top-level instruction. Without
//    walking inner instructions, these pool creations are silently missed.
// 5. Dispatch to the appropriate protocol parser:
//      • PumpSwap  — discriminator + account layout
//      • Raydium V4 — tag byte + minimum accounts/data length
//      • Generic DEX — heuristic: first non-SOL-native token mint in accounts
// 6. Emit MigrationEvent on success; drop on any parse failure.
//
// Account layout references
// ─────────────────────────
// PumpSwap create_pool (instruction accounts):
//   [0] pool_pda           [1] creator
//   [2] base_mint          [3] quote_mint (WSOL)
//   [4] base_vault         [5] quote_vault
//   ... (remainder are program/sysvar accounts)
//
// Raydium AMM V4 initialize2 (instruction accounts, 0-indexed):
//   [0] token_program      [1] spl_ata_program
//   [2] sys_program        [3] rent
//   [4] amm                [5] amm_authority
//   [6] amm_open_orders    [7] amm_lp_mint
//   [8] amm_coin_mint      ← coin token (often the new token)
//   [9] amm_pc_mint        ← pc token (often WSOL)
//   [10..] serum/other accounts
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::transport::ClientTlsConfig;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdateTransaction,
};
use yellowstone_grpc_proto::solana::storage::confirmed_block::{
    CompiledInstruction, InnerInstructions, Message, TransactionStatusMeta,
};

use crate::config::AppConfig;
use crate::dex_registry;
use crate::safety::DeduplicatorSet;
use crate::tatum::GrpcProvider;
use crate::tui::{DashEvent, DashHandle};

// ── Public types ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MigrationEvent {
    pub token_mint: Pubkey,
    /// Pool address if derivable from the instruction layout; None for
    /// generic events where it cannot be determined without protocol-specific
    /// knowledge. The executor never uses this field (it quotes by mint),
    /// but having it be Option makes the "unknown" state explicit.
    pub pool_address: Option<Pubkey>,
    pub dex_label: String,
    pub signature: String,
    pub slot: u64,
}

/// Coarser classification used by executor/scanner for protocol-specific logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexTarget {
    PumpSwap,
    RaydiumV4,
    Generic,
}

impl MigrationEvent {
    pub fn dex_target(&self) -> DexTarget {
        match self.dex_label.as_str() {
            "PumpSwap"   => DexTarget::PumpSwap,
            "Raydium V4" => DexTarget::RaydiumV4,
            _            => DexTarget::Generic,
        }
    }
}

// ── Known instruction discriminators ────────────────────────────────

/// PumpSwap `create_pool` discriminator: sha256("global:create_pool")[..8]
const PUMPSWAP_CREATE_POOL_DISC: [u8; 8] = [0xe9, 0x92, 0xd1, 0x8e, 0xcf, 0x6c, 0xb4, 0x8a];

/// Raydium AMM V4 `initialize2` tag = 1, min 18 bytes data, min 21 accounts.
const RAYDIUM_V4_INIT2_TAG:           u8    = 1;
const RAYDIUM_V4_INIT2_MIN_DATA_LEN:  usize = 18;
const RAYDIUM_V4_INIT2_MIN_ACCOUNTS:  usize = 21;

/// Well-known system/sysvar pubkeys used to skip non-token accounts in the
/// generic heuristic.
const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

// ── Entry point ──────────────────────────────────────────────────────

pub async fn run(
    config: Arc<AppConfig>,
    tx: mpsc::Sender<MigrationEvent>,
    cancel: CancellationToken,
    dash: DashHandle,
) -> Result<()> {
    info!(endpoint = %config.grpc_endpoint, "Listener starting");

    let dedupe = Arc::new(DeduplicatorSet::new(300));
    dedupe.spawn_cleanup(cancel.clone());

    // Build the gRPC provider.  If TATUM_API_KEY is set in config, route
    // through the Tatum managed gateway; otherwise use the custom endpoint.
    let provider = crate::tatum::provider_from_config(
        &config.grpc_endpoint,
        &config.grpc_x_token,
        &config.tatum_api_key,
        &config.tatum_grpc_endpoint,
    )?;

    let dex_map = build_dex_lookup();

    // Build the subscription request once — it is immutable across reconnects
    // because the DEX registry is a static slice. Cloning it is cheap.
    let subscribe_request = build_subscribe_request();

    let mut reconnect_attempt: u32 = 0;
    // Consecutive error count — reset on any clean exit; drives exponential backoff.
    let mut consecutive_errors: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let result = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            r = run_subscription(&provider, &tx, &dedupe, &dex_map, &dash, subscribe_request.clone()) => r,
        };

        match result {
            Ok(()) => {
                // Clean exit (server closed the stream). Reset error backoff and
                // reconnect quickly — this is normal behaviour on rolling restarts.
                consecutive_errors = 0;
                reconnect_attempt  = reconnect_attempt.saturating_add(1);
                warn!("gRPC stream ended cleanly — reconnecting in 2s");
                dash.send(DashEvent::ListenerReconnecting { attempt: reconnect_attempt });
            }
            Err(ref e) => {
                // Compute delay BEFORE incrementing so the first error gets 5 s
                // (not 10 s).  Schedule: 5 → 10 → 20 → 40 → 60 s (hard cap).
                let delay_secs = (5u64 * (1u64 << consecutive_errors.min(4))).min(60);
                consecutive_errors = consecutive_errors.saturating_add(1);
                reconnect_attempt  = reconnect_attempt.saturating_add(1);
                error!(
                    error            = %e,
                    consecutive_errs = consecutive_errors,
                    delay_secs,
                    "gRPC stream error — reconnecting with backoff"
                );
                dash.send(DashEvent::ListenerReconnecting { attempt: reconnect_attempt });
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {},
                }
                continue;
            }
        }

        // Clean exit delay — short, no backoff needed.
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {},
        }
    }
}

// ── Dex lookup table ─────────────────────────────────────────────────

/// Map from program_id → label for all detectable DEXes.
fn build_dex_lookup() -> HashMap<Pubkey, &'static str> {
    dex_registry::detectable_dexes()
        .iter()
        .map(|d| (d.program_id, d.label))
        .collect()
}

/// Build the gRPC SubscribeRequest once from the DEX registry.
/// One transaction filter is created per detectable DEX program.
fn build_subscribe_request() -> SubscribeRequest {
    let mut transactions: HashMap<String, SubscribeRequestFilterTransactions> = HashMap::new();

    for entry in dex_registry::detectable_dexes() {
        let program_str = entry.program_id.to_string();
        let key = entry.label.to_ascii_lowercase().replace(' ', "_");
        transactions.insert(
            key,
            SubscribeRequestFilterTransactions {
                vote:             Some(false),
                failed:           Some(false),
                // account_required (AND): the program MUST appear in the TX.
                // account_include (OR) with a single account is redundant here —
                // account_required alone is the correct and sufficient filter.
                account_include:  vec![],
                account_exclude:  vec![],
                account_required: vec![program_str],
                signature:        None,
            },
        );
    }

    info!(
        filter_count = transactions.len(),
        "gRPC subscription filters built from DEX registry"
    );

    SubscribeRequest {
        transactions,
        commitment: Some(CommitmentLevel::Processed as i32),
        ping: Some(SubscribeRequestPing { id: 1 }),
        ..Default::default()
    }
}

// ── Subscription loop ────────────────────────────────────────────────

async fn run_subscription(
    provider: &GrpcProvider,
    tx: &mpsc::Sender<MigrationEvent>,
    dedupe: &DeduplicatorSet,
    dex_map: &HashMap<Pubkey, &'static str>,
    dash: &DashHandle,
    request: SubscribeRequest,
) -> Result<()> {
    let x_token = provider.x_token();
    let endpoint = provider.endpoint().to_string();

    // TLS is required for any https:// endpoint (Tatum, QuickNode, Triton, etc.).
    // Without an explicit tls_config() call, tonic uses plain-text HTTP/2 and
    // the connection fails immediately with a transport error.
    let uses_tls = endpoint.starts_with("https://");

    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.clone())?
        .x_token(x_token)?
        // Abort the connect attempt after 10 s instead of hanging indefinitely.
        .connect_timeout(std::time::Duration::from_secs(10))
        // HTTP/2 keepalive pings every 15 s — detects silently-dead connections
        // that would otherwise block the stream.recv() forever.
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        // Consider the connection dead if no keepalive ACK arrives within 5 s.
        .keep_alive_timeout(std::time::Duration::from_secs(5))
        // Send keepalives even when there are no in-flight RPCs.  Required here
        // because the subscription is long-lived with no request traffic.
        .keep_alive_while_idle(true)
        // Disable Nagle's algorithm — gRPC messages should be sent immediately.
        .tcp_nodelay(true);

    if uses_tls {
        builder = builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .context("Failed to configure TLS for gRPC endpoint")?;
    }

    let mut client = builder
        .connect()
        .await
        .context("Failed to connect to gRPC endpoint")?;

    let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    info!(
        provider = %provider.label(),
        dex_count = dex_map.len(),
        "gRPC subscription active — listening on {} DEX programs",
        dex_map.len()
    );
    dash.send(DashEvent::ListenerConnected { endpoint });

    while let Some(msg) = stream.next().await {
        let msg = msg.context("gRPC stream message error")?;

        match msg.update_oneof {
            Some(UpdateOneof::Transaction(tx_update)) => {
                process_transaction(dex_map, tx, dedupe, &tx_update, dash).await;
            }
            Some(UpdateOneof::Ping(_)) => {
                let pong = SubscribeRequest {
                    ping: Some(SubscribeRequestPing { id: 1 }),
                    ..Default::default()
                };
                if let Err(e) = subscribe_tx.send(pong).await {
                    return Err(anyhow::anyhow!("Failed to send gRPC pong: {e}"));
                }
                debug!("Sent gRPC pong");
            }
            _ => {}
        }
    }

    Ok(())
}

// ── Transaction processing ────────────────────────────────────────────

async fn process_transaction(
    dex_map: &HashMap<Pubkey, &'static str>,
    event_tx: &mpsc::Sender<MigrationEvent>,
    dedupe: &DeduplicatorSet,
    tx_update: &SubscribeUpdateTransaction,
    dash: &DashHandle,
) {
    // Extract the transaction info wrapper.
    let tx_info = match &tx_update.transaction {
        Some(t) if !t.is_vote => t,
        _ => return,
    };

    // Signature bytes → base58 string for deduplication and event field.
    let signature = bs58::encode(&tx_info.signature).into_string();

    // Deduplicate: the same tx can arrive multiple times from the stream.
    if dedupe.is_duplicate(&signature) {
        return;
    }

    let slot = tx_update.slot;

    // Access the inner transaction proto.
    let proto_tx = match &tx_info.transaction {
        Some(t) => t,
        None    => return,
    };

    let msg = match &proto_tx.message {
        Some(m) => m,
        None    => return,
    };

    // Reject if the tx failed on-chain (meta.err is Some).
    if let Some(meta) = &tx_info.meta {
        if meta.err.is_some() {
            return;
        }
    }

    // Build a flat account table:
    //   static keys (from message.account_keys) followed by ALT-loaded keys
    //   (meta.loaded_writable_addresses then meta.loaded_readonly_addresses).
    let account_table = build_account_table(msg, tx_info.meta.as_ref());

    // Walk top-level instructions first.
    if let Some(event) = scan_instructions(
        msg.instructions.iter(),
        dex_map,
        &account_table,
        &signature,
        slot,
    ) {
        emit_event(event, event_tx, dash);
        return;
    }

    // Walk inner instructions (CPIs).
    //
    // Many launchers and bundlers invoke pool creation via CPI rather than as
    // a top-level instruction. Without this pass, any pool creation that is
    // a cross-program invocation is silently missed. This is the primary
    // reason early-detection bots miss PumpSwap pools created by aggregators.
    if let Some(meta) = &tx_info.meta {
        if let Some(event) = scan_inner_instructions(
            &meta.inner_instructions,
            dex_map,
            &account_table,
            &signature,
            slot,
        ) {
            emit_event(event, event_tx, dash);
        }
    }
}

/// Emit a MigrationEvent to the scanner channel and log it to the dashboard.
fn emit_event(
    event: MigrationEvent,
    event_tx: &mpsc::Sender<MigrationEvent>,
    dash: &DashHandle,
) {
    debug!(
        signature = %event.signature,
        dex       = %event.dex_label,
        token     = %event.token_mint,
        "Pool creation detected"
    );

    dash.send(DashEvent::PoolDetected {
        token: event.token_mint.to_string(),
        dex:   event.dex_label.clone(),
        slot:  event.slot,
    });

    // Drop if scanner is backlogged (channel at capacity).
    let _ = event_tx.try_send(event);
}

/// Scan a sequence of compiled instructions for pool creation events.
/// Returns the first match, or None.
fn scan_instructions<'a>(
    ixs: impl Iterator<Item = &'a CompiledInstruction>,
    dex_map: &HashMap<Pubkey, &'static str>,
    account_table: &[Pubkey],
    signature: &str,
    slot: u64,
) -> Option<MigrationEvent> {
    for ix in ixs {
        let program_id = account_index(account_table, ix.program_id_index as usize)?;

        let dex_label = match dex_map.get(&program_id) {
            Some(label) => *label,
            None        => continue,
        };

        let maybe_event = match dex_label {
            "PumpSwap"   => parse_pumpswap(ix, account_table, signature, slot),
            "Raydium V4" => parse_raydium_v4(ix, account_table, signature, slot),
            _            => parse_generic(ix, dex_label, account_table, signature, slot),
        };

        if maybe_event.is_some() {
            return maybe_event;
        }
    }
    None
}

/// Scan inner instruction groups (CPIs) for pool creation events.
/// Returns the first match, or None.
///
/// In yellowstone-grpc-proto 12.x `InnerInstruction` exposes fields directly
/// (program_id_index, accounts, data) without wrapping a CompiledInstruction.
///
/// Performance note: we pre-filter by DEX program ID *before* constructing a
/// CompiledInstruction.  A typical transaction has many CPI instructions
/// (SPL-token transfers, system transfers, etc.) that do not target any DEX.
/// Skipping the accounts/data clone for non-matching instructions avoids
/// unnecessary heap allocations on the hot path.
fn scan_inner_instructions(
    inner_groups: &[InnerInstructions],
    dex_map: &HashMap<Pubkey, &'static str>,
    account_table: &[Pubkey],
    signature: &str,
    slot: u64,
) -> Option<MigrationEvent> {
    for group in inner_groups {
        for inner in &group.instructions {
            // Resolve the program ID first — no allocation needed.
            let program_id = match account_index(account_table, inner.program_id_index as usize) {
                Some(pk) => pk,
                None     => continue,
            };

            let dex_label = match dex_map.get(&program_id) {
                Some(label) => *label,
                None        => continue,  // Not a DEX we monitor — skip
            };

            // Only now construct the CompiledInstruction (clones accounts+data).
            // This is reached only for instructions that target a known DEX,
            // which is rare in the CPI tree of a typical transaction.
            let ix = CompiledInstruction {
                program_id_index: inner.program_id_index,
                accounts:         inner.accounts.clone(),
                data:             inner.data.clone(),
            };

            let maybe_event = match dex_label {
                "PumpSwap"   => parse_pumpswap(&ix, account_table, signature, slot),
                "Raydium V4" => parse_raydium_v4(&ix, account_table, signature, slot),
                _            => parse_generic(&ix, dex_label, account_table, signature, slot),
            };

            if maybe_event.is_some() {
                return maybe_event;
            }
        }
    }
    None
}

// ── Account table helpers ────────────────────────────────────────────

/// Flat list of all accounts visible to the tx:
///   [static_keys…, writable_alt_keys…, readonly_alt_keys…]
fn build_account_table(msg: &Message, meta: Option<&TransactionStatusMeta>) -> Vec<Pubkey> {
    let mut table = Vec::with_capacity(
        msg.account_keys.len()
            + meta.map(|m| m.loaded_writable_addresses.len() + m.loaded_readonly_addresses.len())
                .unwrap_or(0),
    );

    for raw in &msg.account_keys {
        if let Ok(pk) = pubkey_from_bytes(raw) {
            table.push(pk);
        }
    }

    if let Some(meta) = meta {
        for raw in &meta.loaded_writable_addresses {
            if let Ok(pk) = pubkey_from_bytes(raw) {
                table.push(pk);
            }
        }
        for raw in &meta.loaded_readonly_addresses {
            if let Ok(pk) = pubkey_from_bytes(raw) {
                table.push(pk);
            }
        }
    }

    table
}

/// Resolve an account-table index to a Pubkey reference.
fn account_index(table: &[Pubkey], idx: usize) -> Option<Pubkey> {
    table.get(idx).copied()
}

/// Resolve a single byte (compact account index in instruction.accounts) to a Pubkey.
fn resolve_account(table: &[Pubkey], ix_accounts: &[u8], pos: usize) -> Option<Pubkey> {
    let idx = *ix_accounts.get(pos)? as usize;
    table.get(idx).copied()
}

fn pubkey_from_bytes(raw: &[u8]) -> Result<Pubkey> {
    let arr: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("Expected 32-byte pubkey, got {}", raw.len()))?;
    Ok(Pubkey::new_from_array(arr))
}

// ── Protocol-specific parsers ────────────────────────────────────────

/// PumpSwap `create_pool` parser.
///
/// Instruction account layout:
///   [0] pool_pda    [1] creator
///   [2] base_mint   ← the newly listed token
///   [3] quote_mint  (WSOL)
///   [4] base_vault  [5] quote_vault
///
/// We verify the 8-byte Anchor discriminator and extract accounts[0] as
/// pool_address and accounts[2] as the token mint.
fn parse_pumpswap(
    ix: &CompiledInstruction,
    table: &[Pubkey],
    signature: &str,
    slot: u64,
) -> Option<MigrationEvent> {
    // Minimum data: 8-byte discriminator
    if ix.data.len() < 8 {
        return None;
    }
    if ix.data[..8] != PUMPSWAP_CREATE_POOL_DISC {
        return None;
    }

    // Minimum 6 account indices
    if ix.accounts.len() < 6 {
        return None;
    }

    let pool_address = resolve_account(table, &ix.accounts, 0)?;
    let token_mint   = resolve_account(table, &ix.accounts, 2)?;

    // Sanity: base_mint must not be WSOL (it is the new token)
    let wsol = Pubkey::from_str(WSOL_MINT_STR).ok()?;
    if token_mint == wsol {
        return None;
    }

    Some(MigrationEvent {
        token_mint,
        pool_address: Some(pool_address),
        dex_label: "PumpSwap".into(),
        signature: signature.to_string(),
        slot,
    })
}

/// Raydium AMM V4 `initialize2` parser.
///
/// Instruction data layout: [tag: u8, nonce: u8, open_time: u64, ...]
/// We check:
///   - data[0] == 1 (initialize2 tag)
///   - data.len() >= 18
///   - accounts.len() >= 21
///   - accounts[8] = coin_mint (token), accounts[9] = pc_mint (usually WSOL)
fn parse_raydium_v4(
    ix: &CompiledInstruction,
    table: &[Pubkey],
    signature: &str,
    slot: u64,
) -> Option<MigrationEvent> {
    if ix.data.len() < RAYDIUM_V4_INIT2_MIN_DATA_LEN {
        return None;
    }
    if ix.data[0] != RAYDIUM_V4_INIT2_TAG {
        return None;
    }
    if ix.accounts.len() < RAYDIUM_V4_INIT2_MIN_ACCOUNTS {
        return None;
    }

    // amm (accounts[4]) is the pool address in the Raydium layout.
    let pool_address = resolve_account(table, &ix.accounts, 4)?;
    let coin_mint    = resolve_account(table, &ix.accounts, 8)?;
    let pc_mint      = resolve_account(table, &ix.accounts, 9)?;

    let wsol = Pubkey::from_str(WSOL_MINT_STR).ok()?;

    // The non-SOL leg is the new token. Raydium pairs can be coin/WSOL or
    // WSOL/coin depending on listing direction. Pick the non-WSOL side.
    let token_mint = if coin_mint != wsol {
        coin_mint
    } else if pc_mint != wsol {
        pc_mint
    } else {
        // Both are WSOL — skip (shouldn't happen in practice)
        return None;
    };

    Some(MigrationEvent {
        token_mint,
        pool_address: Some(pool_address),
        dex_label: "Raydium V4".into(),
        signature: signature.to_string(),
        slot,
    })
}

/// Generic DEX parser for all other detectable protocols.
///
/// Heuristic:
///   - Walk the instruction's account indices.
///   - The first non-WSOL, non-system/sysvar 32-byte pubkey that looks like
///     a mint (neither a known system program nor the DEX program itself) is
///     treated as the new token mint.
///   - pool_address is None because the real pool PDA is not derivable
///     without protocol-specific knowledge. This is made explicit with
///     Option<Pubkey> rather than using program_id as a misleading placeholder.
///
/// This is intentionally coarse — it will generate some false positives on
/// non-pool instructions, but those will be filtered out by the scanner when
/// Jupiter cannot quote them.
fn parse_generic(
    ix: &CompiledInstruction,
    dex_label: &'static str,
    table: &[Pubkey],
    signature: &str,
    slot: u64,
) -> Option<MigrationEvent> {
    let wsol     = Pubkey::from_str(WSOL_MINT_STR).ok()?;
    let system   = solana_system_interface::program::id();
    let token_p  = crate::config::programs::token_program();
    let ata_p    = crate::config::programs::ata_program();

    // The set of accounts to skip when hunting for the token mint.
    let skip = [wsol, system, token_p, ata_p];

    let program_id = account_index(table, ix.program_id_index as usize)?;

    // Scan account indices encoded in the instruction.
    for &idx_byte in &ix.accounts {
        let pk = match table.get(idx_byte as usize) {
            Some(pk) => *pk,
            None     => continue,
        };

        // Skip the DEX program itself, system programs, and WSOL.
        if pk == program_id || skip.contains(&pk) {
            continue;
        }

        return Some(MigrationEvent {
            token_mint:  pk,
            // pool_address is unknown for generic events — the real pool PDA
            // is not derivable without protocol-specific knowledge.
            // The scanner quotes by mint, not pool address.
            pool_address: None,
            dex_label: dex_label.to_string(),
            signature: signature.to_string(),
            slot,
        });
    }

    None
}
