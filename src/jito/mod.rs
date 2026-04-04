use anyhow::{Context, Result};
use reqwest::Client;
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use solana_system_interface::instruction as system_instruction;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::warn;

/// Process-wide shared HTTP client for Jito — avoids TLS session re-establishment.
/// 5-second timeout prevents blocking on congested mobile networks.
fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build Jito HTTP client")
    })
}

/// Build a Jito tip instruction: transfer `tip_lamports` from `payer` to a
/// randomly selected Jito tip account.
pub fn build_tip_instruction(payer: &Pubkey, tip_lamports: u64) -> Result<Instruction> {
    use rand::RngExt;
    let tip_accounts = crate::config::programs::jito_tip_accounts();
    // Use a random index to distribute tips across all Jito tip accounts.
    let idx = rand::rng().random_range(0..tip_accounts.len());
    let tip_account = tip_accounts[idx];
    Ok(system_instruction::transfer(payer, &tip_account, tip_lamports))
}

/// Lightweight clone: pointer to shared static client + Arc<str> for URL.
#[derive(Clone)]
pub struct JitoClient {
    http: &'static Client,
    bundle_url: Arc<str>,
}

impl JitoClient {
    pub fn new(url: &str) -> Self {
        Self {
            http: shared_http_client(),
            bundle_url: format!("{}/api/v1/bundles", url.trim_end_matches('/')).into(),
        }
    }

    /// Submit a bundle to Jito. Returns the bundle ID string on success.
    pub async fn send_bundle(&self, txs: &[VersionedTransaction]) -> Result<String> {
        let encoded: Vec<String> = txs.iter()
            .map(|tx| {
                let bytes = bincode1::serialize(tx).context("bincode1 serialize")?;
                Ok(bs58::encode(bytes).into_string())
            })
            .collect::<Result<_>>()?;

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [encoded],
        });

        let resp: serde_json::Value = self.http
            .post(self.bundle_url.as_ref())
            .json(&body)
            .send()
            .await
            .context("Jito HTTP send")?
            .error_for_status()
            .context("Jito HTTP status")?
            .json()
            .await
            .context("Jito JSON parse")?;

        if let Some(err) = resp.get("error") {
            warn!(error = %err, "Jito bundle error response");
            return Err(anyhow::anyhow!("Jito error: {err}"));
        }

        let result = resp.get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| anyhow::anyhow!("Jito response missing 'result' field"))?;
        Ok(result.to_string())
    }
}
