// src/jupiter/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Jupiter V6 API client — quoting and swap instruction generation.
//
// Flow: /quote → /swap-instructions → deserialize into Solana IXs.
//
// NOTE: The public https://quote-api.jup.ag/v6 endpoint is deprecated.
// Self-host the Jupiter V6 Swap API for production use.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::{collections::HashSet, str::FromStr};
use tracing::{debug, warn};

// ── Client ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct JupiterClient {
    http: Client,
    /// Pre-built URL strings — avoids one format!() per request.
    quote_url: String,
    swap_ix_url: String,
}

impl JupiterClient {
    pub fn new(base_url: &str) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|e| {
                warn!(error = %e, "Failed to build tuned Jupiter HTTP client, using default");
                Client::new()
            });

        let base = base_url.trim_end_matches('/');
        Self {
            http,
            quote_url:   format!("{base}/quote"),
            swap_ix_url: format!("{base}/swap-instructions"),
        }
    }

    pub async fn quote(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let resp = self
            .http
            .get(&self.quote_url)
            .query(&[
                ("inputMint",          input_mint.to_string()),
                ("outputMint",         output_mint.to_string()),
                ("amount",             amount.to_string()),
                ("slippageBps",        slippage_bps.to_string()),
                ("onlyDirectRoutes",   "false".into()),
                ("asLegacyTransaction","false".into()),
                ("maxAccounts",        "40".into()),
            ])
            .send()
            .await
            .context("Jupiter quote request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Jupiter /quote failed ({status}): {body}");
        }

        resp.json::<QuoteResponse>()
            .await
            .context("Failed to parse Jupiter quote response")
    }

    /// Fetch decomposed swap instructions so we can compose them atomically
    /// with flash loan borrow/repay in a single transaction.
    pub async fn swap_instructions(
        &self,
        user_pubkey: &Pubkey,
        quote_response: &QuoteResponse,
    ) -> Result<SwapInstructionsResponse> {
        let quote_json = serde_json::to_value(quote_response)
            .context("Failed to serialize quote response")?;

        let request = SwapInstructionsRequest {
            user_public_key: user_pubkey.to_string(),
            quote_response: quote_json,
            // MUST be false for flash-loan context.
            //
            // The flash-loan borrow puts WSOL into the fee-payer's ATA; the
            // flash-loan repay pulls WSOL back out of the same ATA.  Setting
            // wrap_and_unwrap_sol = true would cause Jupiter to append an
            // "unwrap WSOL → native SOL" instruction after the sell swap,
            // emptying the ATA before the repay instruction can draw from it —
            // the repay then fails with an insufficient-funds error.
            wrap_and_unwrap_sol: Some(false),
            compute_unit_price_micro_lamports: None,
            as_legacy_transaction: Some(false),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: None,
        };

        let resp = self
            .http
            .post(&self.swap_ix_url)
            .json(&request)
            .send()
            .await
            .context("Jupiter swap-instructions request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Jupiter /swap-instructions failed ({status}): {body}");
        }

        resp.json::<SwapInstructionsResponse>()
            .await
            .context("Failed to parse swap-instructions response")
    }

    /// Fetch buy and sell swap instructions concurrently.
    pub async fn swap_instructions_pair(
        &self,
        user_pubkey: &Pubkey,
        buy_quote: &QuoteResponse,
        sell_quote: &QuoteResponse,
    ) -> Result<(SwapInstructionsResponse, SwapInstructionsResponse)> {
        let (buy_res, sell_res) = tokio::try_join!(
            self.swap_instructions(user_pubkey, buy_quote),
            self.swap_instructions(user_pubkey, sell_quote),
        )?;
        Ok((buy_res, sell_res))
    }
}

// ── Conversion helpers ──────────────────────────────────────────────

/// Convert a Jupiter `InstructionData` into a Solana SDK `Instruction`.
/// Returns an error on invalid pubkeys or base64 data (no panics).
pub fn to_solana_instruction(ix_data: &InstructionData) -> Result<Instruction> {
    let program_id = Pubkey::from_str(&ix_data.program_id)
        .context("Invalid program_id in Jupiter instruction")?;

    let accounts: Result<Vec<AccountMeta>> = ix_data
        .accounts
        .iter()
        .map(|acc| {
            let pubkey = Pubkey::from_str(&acc.pubkey)
                .with_context(|| format!("Invalid pubkey in Jupiter instruction: {}", acc.pubkey))?;
            Ok(if acc.is_writable {
                AccountMeta::new(pubkey, acc.is_signer)
            } else {
                AccountMeta::new_readonly(pubkey, acc.is_signer)
            })
        })
        .collect();

    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&ix_data.data)
        .context("Failed to decode Jupiter instruction data")?;

    Ok(Instruction { program_id, accounts: accounts?, data })
}

/// Collect all ALT addresses referenced by two swap-instruction responses.
/// Uses a HashSet for O(1) deduplication instead of O(n) Vec::contains.
pub fn collect_alt_addresses(
    buy: &SwapInstructionsResponse,
    sell: &SwapInstructionsResponse,
) -> Result<Vec<Pubkey>> {
    let mut seen = HashSet::new();
    let mut alts = Vec::new();

    for response in [buy, sell] {
        for addr_str in response.address_lookup_table_addresses.iter().flatten() {
            let pk = Pubkey::from_str(addr_str)
                .with_context(|| format!("Invalid ALT address: {addr_str}"))?;
            if seen.insert(pk) {
                alts.push(pk);
            }
        }
    }
    Ok(alts)
}

pub fn parse_out_amount(quote: &QuoteResponse) -> Result<u64> {
    quote
        .out_amount
        .parse::<u64>()
        .context("Failed to parse out_amount from Jupiter quote")
}

/// Estimate net profit: borrow SOL → buy token → sell token → repay.
/// Returns net in lamports (negative = loss).
///
/// # Conservative by design
///
/// We use `other_amount_threshold` from the sell quote (the minimum guaranteed
/// output after slippage) rather than `out_amount` (the optimistic expected
/// output). Using the optimistic figure causes the scanner to greenlight trades
/// that will be unprofitable or lossy once real slippage is applied at execution.
///
/// This intentionally under-estimates profit on low-slippage opportunities,
/// meaning some borderline trades will be skipped. That is the correct
/// trade-off: a missed profitable trade costs opportunity; an executed losing
/// trade costs real SOL.
pub fn estimate_profit(
    borrow_amount: u64,
    _buy_quote: &QuoteResponse,
    sell_quote: &QuoteResponse,
    flash_loan_fee_bps: u16,
    tx_cost_lamports: u64,
) -> Result<i64> {
    // Worst-case SOL returned after sell slippage.
    // other_amount_threshold = floor(out_amount × (1 - slippage_bps/10000))
    let sol_received: u64 = sell_quote
        .other_amount_threshold
        .parse()
        .context("Failed to parse other_amount_threshold from sell quote")?;

    let flash_loan_fee = (borrow_amount as u128 * flash_loan_fee_bps as u128 / 10_000) as u64;
    let total_repay    = borrow_amount.saturating_add(flash_loan_fee);

    // Safe cast: max_loan_lamports (50 SOL = 50_000_000_000) << i64::MAX (9.2e18).
    let net = sol_received as i64 - total_repay as i64 - tx_cost_lamports as i64;

    debug!(
        sol_received,
        borrow_amount,
        flash_loan_fee,
        tx_cost_lamports,
        net,
        "estimate_profit (conservative: using other_amount_threshold)"
    );

    Ok(net)
}

// ── API types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapInstructionsRequest {
    user_public_key: String,
    quote_response: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    wrap_and_unwrap_sol: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price_micro_lamports: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_legacy_transaction: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dynamic_compute_unit_limit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prioritization_fee_lamports: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    /// Minimum output guaranteed after slippage. Used as the conservative
    /// profit estimate in estimate_profit().
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    pub price_impact_pct: String,
    pub route_plan: Vec<RoutePlanStep>,
    #[serde(default)]
    pub context_slot: u64,
    #[serde(default)]
    pub time_taken: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: Option<String>,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub fee_amount: String,
    pub fee_mint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    pub token_ledger_instruction: Option<InstructionData>,
    pub compute_budget_instructions: Option<Vec<InstructionData>>,
    pub setup_instructions: Option<Vec<InstructionData>>,
    pub swap_instruction: InstructionData,
    pub cleanup_instruction: Option<InstructionData>,
    pub address_lookup_table_addresses: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InstructionData {
    pub program_id: String,
    pub accounts: Vec<AccountKeyData>,
    pub data: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountKeyData {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}
