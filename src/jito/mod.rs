// src/jito/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Jito bundle submission via JSON-RPC.
//
// Key design decisions:
//   1. Parallel multi-region fan-out — bundles are sent to all five Jito
//      regional block engine endpoints simultaneously. The first bundle_id
//      that comes back is used for status polling. This hedges leader
//      geography: a given slot leader is always closest to one region, and
//      parallel submission ensures we always hit that closest path.
//
//   2. Tight 4-second confirmation timeout — a bundle either lands within
//      ~10 slots (4s) or the opportunity window is gone. Polling past that
//      wastes time that could be spent on the next opportunity.
//
//   3. Dynamic tip — callers pass the computed tip; this module just embeds
//      it as a SOL transfer instruction to a random tip account.
//
//   4. Shared HTTP client pool — a single `reqwest::Client` is constructed
//      once per process (OnceLock) and shared across all regional sends,
//      avoiding per-call TCP handshakes.
//
// Transaction encoding:
//   Solana and Jito expect transactions encoded with bincode 1.x (fixed-int,
//   little-endian). bincode 2.x standard() uses varint encoding — a different
//   wire format — which causes Jito to silently reject bundles. We use the
//   `bincode1` crate alias (see Cargo.toml) exclusively for this purpose.
//
// Endpoints:
//   POST /api/v1/bundles  { method: "sendBundle",        params: [[base58_tx]] }
//   POST /api/v1/bundles  { method: "getBundleStatuses", params: [[id]]        }
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use futures::future::select_ok;
use rand::RngExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use solana_system_interface::instruction as system_instruction;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

use crate::config::programs;

// ── Jito regional block engine endpoints ────────────────────────────
// Bundles are sent to all regions in parallel. Different slot leaders
// are co-located with different regions on any given slot.

const BLOCK_ENGINE_REGIONS: &[&str] = &[
    "https://mainnet.block-engine.jito.wtf",
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://frankfurt.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
];

// ── Shared HTTP client (built once, reused for all requests) ─────────

/// Returns a reference to the process-wide reqwest::Client.
///
/// Building a Client is expensive (opens connection pool, loads TLS roots).
/// By sharing one instance across all regional sends we avoid re-paying that
/// cost on every arb attempt (was previously 5× per arb).
fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|e| {
                warn!(error = %e, "Failed to build Jito HTTP client, using default");
                Client::new()
            })
    })
}

// ── Tip instruction ─────────────────────────────────────────────────

/// Build a SOL transfer instruction to a randomly selected Jito tip account.
/// Include as the last instruction in the arb transaction.
pub fn build_tip_instruction(payer: &Pubkey, tip_lamports: u64) -> Result<Instruction> {
    // jito_tip_accounts() is infallible — returns [Pubkey; 8] directly via
    // OnceLock. The ? operator is invalid here. The explicit type annotation
    // resolves the E0282 type-inference failure on tip_accounts.len() below.
    let tip_accounts: [solana_sdk::pubkey::Pubkey; 8] = programs::jito_tip_accounts();
    let idx = rand::rng().random_range(0..tip_accounts.len());
    let tip_account = tip_accounts[idx];

    debug!(tip_account = %tip_account, tip_lamports, "Selected Jito tip account");

    Ok(system_instruction::transfer(payer, &tip_account, tip_lamports))
}

// ── Transaction encoding ─────────────────────────────────────────────

/// Encode a VersionedTransaction to the bincode 1.x wire format expected by
/// Solana validators and the Jito block engine.
///
/// # Why bincode 1.x
///
/// The Solana protocol uses bincode 1.x (fixed-width integers, little-endian)
/// for transaction serialization. bincode 2.x `standard()` config uses varint
/// encoding — a completely different wire format — which causes Jito to reject
/// bundles silently (the bundle is accepted at the HTTP layer but never lands).
///
/// We use the `bincode1` crate alias (see Cargo.toml) to ensure the correct
/// encoding is always used, independent of which bincode version is imported
/// elsewhere.
fn encode_tx_for_jito(tx: &VersionedTransaction) -> Result<String> {
    // bincode1 = package "bincode" version "=1.3.3" — the Solana-compatible format.
    let bytes = bincode1::serialize(tx)
        .context("Failed to serialize VersionedTransaction with bincode 1.x")?;
    Ok(bs58::encode(bytes).into_string())
}

// ── Client ──────────────────────────────────────────────────────────

pub struct JitoClient {
    /// Reference to the shared HTTP client — zero allocation.
    http: &'static Client,
    /// Pre-built bundle URL for this specific endpoint.
    bundle_url: String,
}

impl JitoClient {
    pub fn new(block_engine_url: &str) -> Self {
        Self {
            http: shared_http_client(),
            bundle_url: format!("{}/api/v1/bundles", block_engine_url.trim_end_matches('/')),
        }
    }

    /// Send a bundle to this single endpoint. Returns the bundle_id string.
    async fn send_bundle_inner(&self, encoded_txs: &[String]) -> Result<String> {
        #[derive(Serialize)]
        struct Req<'a> {
            jsonrpc: &'static str,
            id: u64,
            method: &'static str,
            params: [&'a [String]; 1],
        }
        #[derive(Deserialize)]
        struct Resp<T> {
            result: Option<T>,
            error:  Option<RpcError>,
        }
        #[derive(Deserialize)]
        struct RpcError {
            code: i64,
            message: String,
        }

        let req = Req {
            jsonrpc: "2.0",
            id:      1,
            method:  "sendBundle",
            params:  [encoded_txs],
        };

        let body: Resp<String> = self
            .http
            .post(&self.bundle_url)
            .json(&req)
            .send()
            .await
            .context("Jito sendBundle HTTP request failed")?
            .error_for_status()
            .context("Jito sendBundle returned HTTP error")?
            .json()
            .await
            .context("Failed to decode Jito sendBundle response")?;

        if let Some(err) = body.error {
            anyhow::bail!("Jito sendBundle RPC error {}: {}", err.code, err.message);
        }

        body.result.context("Jito sendBundle missing result")
    }

    /// Poll for bundle status on this endpoint. Returns `BundleOutcome`.
    ///
    /// Timeout is 4 seconds — roughly 10 slots. If a bundle hasn't landed
    /// within that window the opportunity is stale regardless.
    pub async fn wait_for_bundle(&self, bundle_id: &str) -> Result<BundleOutcome> {
        #[derive(Serialize)]
        struct Req<'a> {
            jsonrpc: &'static str,
            id:      u64,
            method:  &'static str,
            params:  [[&'a str; 1]; 1],
        }
        #[derive(Deserialize)]
        struct Resp {
            result: Option<Statuses>,
            error:  Option<RpcError>,
        }
        #[derive(Deserialize)]
        struct RpcError {
            code: i64,
            message: String,
        }
        #[derive(Deserialize)]
        struct Statuses {
            value: Vec<Option<Status>>,
        }
        #[derive(Deserialize)]
        struct Status {
            confirmation_status: Option<String>,
            err:                 Option<serde_json::Value>,
            transactions:        Option<Vec<String>>,
        }

        let started = std::time::Instant::now();
        // 4 seconds = ~10 Solana slots. Beyond this the opportunity window is closed.
        let timeout = std::time::Duration::from_secs(4);
        // Start polling after one slot (~400ms) — the earliest a bundle can land.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        loop {
            if started.elapsed() > timeout {
                return Ok(BundleOutcome::Timeout);
            }

            let req = Req {
                jsonrpc: "2.0",
                id:      1,
                method:  "getBundleStatuses",
                params:  [[bundle_id]],
            };

            // Transient HTTP/network errors are swallowed: we log at warn and
            // continue polling until the timeout.  Only two conditions cause
            // an early exit:
            //   • An RPC-level error in the JSON response body (bundle failed)
            //   • The bundle reaches confirmed/finalized status
            // Everything else (connection resets, 5xx, decode failures) is
            // transient and must not be allowed to lose a bundle that may
            // already be on-chain.
            let body_opt: Option<Resp> = match self
                .http
                .post(&self.bundle_url)
                .json(&req)
                .send()
                .await
            {
                Err(e) => {
                    warn!(bundle_id, error = %e, "getBundleStatuses HTTP error — retrying");
                    None
                }
                Ok(resp) => match resp.error_for_status() {
                    Err(e) => {
                        warn!(bundle_id, error = %e, "getBundleStatuses HTTP status error — retrying");
                        None
                    }
                    Ok(r) => match r.json::<Resp>().await {
                        Err(e) => {
                            warn!(bundle_id, error = %e, "getBundleStatuses decode error — retrying");
                            None
                        }
                        Ok(b) => Some(b),
                    },
                },
            };

            if let Some(body) = body_opt {
                if let Some(err) = body.error {
                    // RPC-level error is definitive: the bundle failed or is
                    // unknown.  No point continuing to poll.
                    anyhow::bail!(
                        "Jito getBundleStatuses RPC error {}: {}",
                        err.code, err.message
                    );
                }

                if let Some(result) = body.result {
                    if let Some(Some(status)) = result.value.into_iter().next() {
                        if let Some(err) = status.err {
                            return Ok(BundleOutcome::Failed { reason: err.to_string() });
                        }
                        if let Some(cs) = status.confirmation_status {
                            // The Jito getBundleStatuses API returns the confirmation_status
                            // field as a JSON string matching the Solana RPC spec:
                            // "processed" | "confirmed" | "finalized"
                            let lower = cs.to_ascii_lowercase();
                            if lower == "confirmed" || lower == "finalized" {
                                let signature = status
                                    .transactions
                                    .and_then(|mut v| v.drain(..).next())
                                    .unwrap_or_default();
                                info!(bundle_id, status = %lower, "Jito bundle confirmed");
                                return Ok(BundleOutcome::Landed { signature });
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    }
}

// ── Multi-region parallel submission ────────────────────────────────

/// Encode transactions once, then fan-out to all five Jito regional block
/// engine endpoints in parallel. Returns the bundle_id and a `JitoClient`
/// pointed at the winning endpoint (for status polling).
///
/// All regional clients share the same underlying `reqwest::Client` so no
/// additional TCP connections are opened beyond what the connection pool
/// already manages.
pub async fn send_bundle_parallel(
    transactions: &[&VersionedTransaction],
) -> Result<(String, JitoClient)> {
    if transactions.is_empty() {
        anyhow::bail!("Cannot send empty Jito bundle");
    }

    // Encode once using bincode 1.x — the format Jito expects.
    // See encode_tx_for_jito() for the full rationale.
    let encoded_txs: Vec<String> = transactions
        .iter()
        .map(|tx| encode_tx_for_jito(tx))
        .collect::<Result<_>>()?;

    // Each future returns (bundle_id, &'static str url) on success so the
    // winner carries the URL needed to build the polling client.
    let futures: Vec<_> = BLOCK_ENGINE_REGIONS
        .iter()
        .map(|&url| {
            let txs = encoded_txs.clone();
            // JitoClient::new is cheap — just assigns the static ref + formats URL.
            let client = JitoClient::new(url);
            Box::pin(async move {
                client
                    .send_bundle_inner(&txs)
                    .await
                    .map(|bundle_id| (bundle_id, url))
            })
        })
        .collect();

    let ((bundle_id, winning_url), _remaining) = select_ok(futures)
        .await
        .context("All Jito block engine endpoints failed")?;

    debug!(endpoint = winning_url, %bundle_id, "Bundle accepted by endpoint");

    Ok((bundle_id, JitoClient::new(winning_url)))
}

// ── Outcome type ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum BundleOutcome {
    Landed { signature: String },
    Failed { reason: String },
    Timeout,
}
