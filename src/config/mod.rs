use anyhow::Result;
use clap::Parser;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::str::FromStr;
use std::sync::OnceLock;

pub mod programs {
    use super::*;
    const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
    const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const JITO_TIP_ACCOUNTS: [&str; 8] = [
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
        "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
        "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    ];

    pub fn wsol_mint() -> Pubkey {
        static WSOL: OnceLock<Pubkey> = OnceLock::new();
        *WSOL.get_or_init(|| Pubkey::from_str(WSOL_MINT_STR).unwrap())
    }

    pub fn ata_program() -> Pubkey {
        static ATA: OnceLock<Pubkey> = OnceLock::new();
        *ATA.get_or_init(|| Pubkey::from_str(ATA_PROGRAM).unwrap())
    }

    pub fn token_program() -> Pubkey {
        static TOKEN: OnceLock<Pubkey> = OnceLock::new();
        *TOKEN.get_or_init(|| Pubkey::from_str(TOKEN_PROGRAM).unwrap())
    }

    pub fn jito_tip_accounts() -> [Pubkey; 8] {
        static TIPS: OnceLock<[Pubkey; 8]> = OnceLock::new();
        *TIPS.get_or_init(|| {
            let mut arr = [Pubkey::default(); 8];
            for (i, s) in JITO_TIP_ACCOUNTS.iter().enumerate() {
                arr[i] = Pubkey::from_str(s).unwrap();
            }
            arr
        })
    }

    pub fn lending_programs() -> Vec<Pubkey> {
        static LENDING: OnceLock<Vec<Pubkey>> = OnceLock::new();
        LENDING.get_or_init(|| {
            vec![
                Pubkey::from_str("So1endDq2Ykq6EB8WnAn8W7THCWYFTFvXrqcnS8tMvS").unwrap(), // Solend
                Pubkey::from_str("KLend2g3cPEPihL362shW35otGoSS6V3fM6fHjr45qG").unwrap(), // Kamino
                Pubkey::from_str("MFv2hWf31Z9kb3u7MqcPySxd9Y6S9Xj6Y9Y9Y9Y9Y9Y").unwrap(), // Marginfi (Placeholder)
            ]
        }).clone()
    }

    pub fn validate_constants() {
        let _ = wsol_mint();
        let _ = ata_program();
        let _ = token_program();
        let _ = jito_tip_accounts();
        let _ = lending_programs();
    }
}

#[derive(Parser, Clone)]
pub struct CliArgs {
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: String,
    #[arg(long, env = "GRPC_ENDPOINT")]
    pub grpc_endpoint: String,
    #[arg(long, env = "GRPC_X_TOKEN", default_value = "")]
    pub grpc_x_token: String,
    #[arg(long, env = "FEE_PAYER_KEYPAIR_BASE58")]
    pub fee_payer_keypair_base58: String,
    #[arg(long, env = "MIN_PROFIT_SOL", default_value = "0.005")]
    pub min_profit_sol: f64,
    #[arg(long, env = "MAX_LOAN_SOL", default_value = "1.0")]
    pub max_loan_sol: f64,
    #[arg(long, env = "SLIPPAGE_BPS", default_value = "100")]
    pub slippage_bps: u16,
    #[arg(long, env = "JUPITER_API_URL", default_value = "http://127.0.0.1:8080")]
    pub jupiter_api_url: String,
    #[arg(long, env = "JITO_BLOCK_ENGINE_URL", default_value = "https://mainnet.block-engine.jito.wtf")]
    pub jito_block_engine_url: String,
    #[arg(long, env = "JITO_TIP_PROFIT_FRACTION", default_value = "0.50")]
    pub jito_tip_profit_fraction: f64,
    #[arg(long, env = "SCANNER_MAX_CONCURRENCY", default_value = "32")]
    pub scanner_max_concurrency: usize,
    #[arg(long, env = "MAX_OPPORTUNITY_AGE_SLOTS", default_value = "20")]
    pub max_opportunity_age_slots: u64,
}

pub struct AppConfig {
    pub rpc_url: String,
    pub grpc_endpoint: String,
    pub grpc_x_token: String,
    pub fee_payer: Keypair,
    pub min_profit_lamports: u64,
    pub max_loan_lamports: u64,
    pub slippage_bps: u16,
    pub jupiter_api_url: String,
    pub jito_block_engine_url: String,
    pub jito_tip_profit_fraction: f64,
    pub scanner_max_concurrency: usize,
    pub max_opportunity_age_slots: u64,
}

impl AppConfig {
    pub fn from_cli(args: CliArgs) -> Result<Self> {
        programs::validate_constants();
        let key_bytes = bs58::decode(&args.fee_payer_keypair_base58).into_vec()?;
        let fee_payer = Keypair::try_from(key_bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("Invalid keypair: {e}"))?;

        Ok(Self {
            rpc_url: args.rpc_url,
            grpc_endpoint: args.grpc_endpoint,
            grpc_x_token: args.grpc_x_token,
            fee_payer,
            min_profit_lamports: (args.min_profit_sol * 1_000_000_000.0) as u64,
            max_loan_lamports: (args.max_loan_sol * 1_000_000_000.0) as u64,
            slippage_bps: args.slippage_bps,
            jupiter_api_url: args.jupiter_api_url,
            jito_block_engine_url: args.jito_block_engine_url,
            jito_tip_profit_fraction: args.jito_tip_profit_fraction,
            scanner_max_concurrency: args.scanner_max_concurrency,
            max_opportunity_age_slots: args.max_opportunity_age_slots,
        })
    }

    pub fn dynamic_jito_tip(&self, profit_lamports: u64) -> u64 {
        (profit_lamports as f64 * self.jito_tip_profit_fraction) as u64
    }

    pub fn estimated_tx_cost(&self) -> u64 {
        15_000
    }
}
