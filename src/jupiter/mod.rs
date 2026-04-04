use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

/// Process-wide shared reqwest client — avoids re-creating TLS sessions per task.
/// Uses a 5-second timeout to prevent blocking on slow mobile networks.
fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client")
    })
}

/// Trimmed quote response — only fields we actually use.
/// Dropping `route_plan: Vec<serde_json::Value>` saves significant heap per quote.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    pub price_impact_pct: String,
    /// Route plan retained for swap-instructions POST body, but stored as raw JSON
    /// to avoid deserializing into typed structs we never inspect.
    #[serde(default)]
    pub route_plan: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    pub swap_instruction: InstructionData,
    pub setup_instructions: Option<Vec<InstructionData>>,
    pub cleanup_instruction: Option<InstructionData>,
    pub address_lookup_table_addresses: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionData {
    pub program_id: String,
    pub accounts: Vec<AccountMetaData>,
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMetaData {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

/// Lightweight clone: only stores a pointer to the shared client + an Arc<str>.
#[derive(Clone)]
pub struct JupiterClient {
    http: &'static Client,
    base_url: std::sync::Arc<str>,
}

impl JupiterClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: shared_http_client(),
            base_url: base_url.trim_end_matches('/').into(),
        }
    }

    #[inline]
    pub async fn quote(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.base_url, input_mint, output_mint, amount, slippage_bps
        );
        let resp = self.http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    #[inline]
    pub async fn swap_instructions(
        &self,
        user_pubkey: &Pubkey,
        quote: &QuoteResponse,
    ) -> Result<SwapInstructionsResponse> {
        let url = format!("{}/swap-instructions", self.base_url);
        let body = serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": user_pubkey.to_string(),
            "wrapAndUnwrapSol": false,
        });
        let resp = self.http
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

#[inline]
pub fn parse_ix(ix: &InstructionData) -> Result<Instruction> {
    let program_id = Pubkey::from_str(&ix.program_id)?;
    let accounts = ix.accounts.iter().map(|a| {
        Ok(AccountMeta {
            pubkey: Pubkey::from_str(&a.pubkey)?,
            is_signer: a.is_signer,
            is_writable: a.is_writable,
        })
    }).collect::<Result<Vec<_>>>()?;
    let data = b64_deserialize(&ix.data)?;
    Ok(Instruction { program_id, accounts, data })
}

#[inline]
fn b64_deserialize(s: &str) -> Result<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose};
    general_purpose::STANDARD.decode(s).context("base64 decode")
}

#[inline]
pub fn estimate_profit(
    loan_amount: u64,
    sell_quote: &QuoteResponse,
    _fee_bps: u16,
    tx_cost: u64,
) -> Result<i64> {
    let out_amount: u64 = sell_quote.other_amount_threshold.parse()?;
    Ok(out_amount as i64 - loan_amount as i64 - tx_cost as i64)
}
