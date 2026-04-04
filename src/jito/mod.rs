use anyhow::{Context, Result};
use reqwest::Client;
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use solana_system_interface::instruction as system_instruction;
use std::sync::OnceLock;

fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| Client::new())
}

pub fn build_tip_instruction(payer: &Pubkey, tip_lamports: u64) -> Result<Instruction> {
    let tip_accounts = crate::config::programs::jito_tip_accounts();
    let idx = (solana_sdk::clock::Slot::default() as usize) % tip_accounts.len();
    let tip_account = tip_accounts[idx];
    Ok(system_instruction::transfer(payer, &tip_account, tip_lamports))
}

#[derive(Clone)]
pub struct JitoClient {
    http: &'static Client,
    bundle_url: String,
}

impl JitoClient {
    pub fn new(url: &str) -> Self {
        Self {
            http: shared_http_client(),
            bundle_url: format!("{}/api/v1/bundles", url.trim_end_matches('/')),
        }
    }

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

        let resp: serde_json::Value = self.http.post(&self.bundle_url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp["result"].as_str().context("Missing result")?.to_string())
    }
}
