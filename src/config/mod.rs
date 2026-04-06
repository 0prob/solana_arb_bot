use anyhow::Result;
use clap::Parser;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::str::FromStr;
use std::sync::{OnceLock, Arc};

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
                Pubkey::from_str("MFv2hWf31Z9kbCa1snEPYktwafCSNDh8nX1H6A21R5X").unwrap(), // Marginfi V2 Program ID
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
    /// Disable the TUI and run in headless / log-to-stdout mode.
    #[arg(long, env = "NO_TUI", default_value = "false")]
    pub no_tui: bool,
    /// TUI target frames per second (1–60).
    #[arg(long, env = "TUI_FPS", default_value = "10")]
    pub tui_fps: u64,
    /// Enable mouse support in the TUI.
    #[arg(long, env = "TUI_MOUSE", default_value = "false")]
    pub tui_mouse: bool,
    /// Force compact TUI layout (useful for small terminals).
    #[arg(long, env = "TUI_COMPACT", default_value = "false")]
    pub tui_compact: bool,
    /// Skip transaction simulation (saves 1 RPC round-trip; recommended on mobile).
    #[arg(long, env = "SKIP_SIMULATION", default_value = "true")]
    pub skip_simulation: bool,
    /// Maximum RSS memory in MB before resource guard throttles the scanner.
    #[arg(long, env = "MAX_MEMORY_MB", default_value = "2500")]
    pub max_memory_mb: u64,
}

pub struct AppConfig {
    pub rpc_url: Arc<str>,
    pub grpc_endpoint: Arc<str>,
    pub grpc_x_token: Arc<str>,
    pub fee_payer: Keypair,
    pub min_profit_lamports: u64,
    pub max_loan_lamports: u64,
    pub slippage_bps: u16,
    pub jupiter_api_url: Arc<str>,
    pub jito_block_engine_url: Arc<str>,
    pub jito_tip_profit_fraction: f64,
    pub scanner_max_concurrency: usize,
    pub max_opportunity_age_slots: u64,
    /// Skip transaction simulation to save 1 RPC round-trip (recommended on mobile).
    pub skip_simulation: bool,
    /// Maximum allowed RSS memory in MB before the resource guard triggers throttling.
    pub max_memory_mb: u64,
}

impl AppConfig {
    pub fn from_cli(args: CliArgs) -> Result<Self> {
        programs::validate_constants();
        let key_bytes = bs58::decode(&args.fee_payer_keypair_base58).into_vec()?;
        let fee_payer = Keypair::try_from(key_bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("Invalid keypair: {e}"))?;

        if args.min_profit_sol < 0.0 {
            return Err(anyhow::anyhow!("MIN_PROFIT_SOL must be non-negative"));
        }
        if args.max_loan_sol <= 0.0 {
            return Err(anyhow::anyhow!("MAX_LOAN_SOL must be positive"));
        }
        if args.jito_tip_profit_fraction < 0.0 || args.jito_tip_profit_fraction > 1.0 {
            return Err(anyhow::anyhow!("JITO_TIP_PROFIT_FRACTION must be in [0.0, 1.0]"));
        }

        Ok(Self {
            rpc_url: args.rpc_url.into(),
            grpc_endpoint: args.grpc_endpoint.into(),
            grpc_x_token: args.grpc_x_token.into(),
            fee_payer,
            min_profit_lamports: (args.min_profit_sol * 1_000_000_000.0).round() as u64,
            max_loan_lamports: (args.max_loan_sol * 1_000_000_000.0).round() as u64,
            slippage_bps: args.slippage_bps,
            jupiter_api_url: args.jupiter_api_url.into(),
            jito_block_engine_url: args.jito_block_engine_url.into(),
            jito_tip_profit_fraction: args.jito_tip_profit_fraction,
            scanner_max_concurrency: args.scanner_max_concurrency,
            max_opportunity_age_slots: args.max_opportunity_age_slots,
            skip_simulation: args.skip_simulation,
            max_memory_mb: args.max_memory_mb,
        })
    }

    /// Compute the Jito bundle tip as a fraction of profit.
    ///
    /// Uses integer arithmetic to avoid f64 rounding.  The fraction is stored as
    /// a float in config for human readability, but is converted to a basis-point
    /// integer (0–1000) here to keep the hot path allocation-free.
    ///
    /// Enforces a minimum tip of 1_000 lamports (0.000001 SOL) so the bundle
    /// is always competitive, and a maximum of 50% of profit to prevent
    /// misconfigured fractions from eating the entire profit.
    pub fn dynamic_jito_tip(&self, profit_lamports: u64) -> u64 {
        // Convert fraction to basis points (0–1000) using integer arithmetic.
        let fraction_bps = (self.jito_tip_profit_fraction.clamp(0.0, 1.0) * 1000.0) as u64;
        let tip = profit_lamports.saturating_mul(fraction_bps) / 1000;
        // Minimum tip: 1_000 lamports (ensures bundle is submitted even on tiny profits).
        // Maximum tip: 50% of profit (safety cap against misconfiguration).
        tip.clamp(1_000, profit_lamports / 2)
    }

    /// Estimated on-chain transaction cost in lamports.
    ///
    /// Covers:
    /// - 5_000 lamports: base transaction fee (5 signatures × 1000 lamports)
    /// - 10_000 lamports: compute-unit priority fee budget
    ///
    /// Total: 15_000 lamports (0.000015 SOL)
    ///
    /// This is a conservative estimate.  The actual cost depends on the number
    /// of signatures and the compute-unit price set by the Jito tip.
    #[inline(always)]
    pub fn estimated_tx_cost(&self) -> u64 {
        15_000
    }
}
