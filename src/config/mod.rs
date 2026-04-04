// src/config/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Configuration — CLI args merged with .env via Clap derive.
// All validated runtime tunables live here.
//
// Security note: AppConfig intentionally does NOT derive Debug.
// The fee_payer Keypair must never appear in log output, panic messages,
// or error traces. The fee_payer_pubkey field (public key only) is safe
// to log and is provided for that purpose.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use clap::Parser;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};
use std::str::FromStr;
use std::sync::OnceLock;

/// Well-known on-chain program/mint addresses used by flash loan and tx building.
/// DEX program IDs live in `dex_registry` instead.
pub mod programs {
    use super::*;

    const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const ATA_PROGRAM:   &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

    // Jito mainnet tip payment accounts.
    // Source: Jito Foundation MEV docs, "On-Chain Addresses".
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

    // ── Infallible getters ───────────────────────────────────────────
    //
    // These addresses are hard-coded constants validated at startup by
    // validate_constants(). After AppConfig::from_cli() succeeds, they
    // are guaranteed to be present in the OnceLock. Callers do NOT need
    // to handle a Result — doing so would add noise with no safety benefit.
    //
    // Do not call these before validate_constants() has run (i.e., before
    // AppConfig::from_cli() returns successfully).

    /// WSOL mint address. Infallible after startup validation.
    pub fn wsol_mint() -> Pubkey {
        static WSOL: OnceLock<Pubkey> = OnceLock::new();
        *WSOL.get_or_init(|| {
            Pubkey::from_str(WSOL_MINT_STR).expect("hard-coded WSOL_MINT_STR is valid")
        })
    }

    /// SPL Token program address. Infallible after startup validation.
    pub fn token_program() -> Pubkey {
        static TOKEN: OnceLock<Pubkey> = OnceLock::new();
        *TOKEN.get_or_init(|| {
            Pubkey::from_str(TOKEN_PROGRAM).expect("hard-coded TOKEN_PROGRAM is valid")
        })
    }

    /// Associated Token Account program address. Infallible after startup validation.
    pub fn ata_program() -> Pubkey {
        static ATA: OnceLock<Pubkey> = OnceLock::new();
        *ATA.get_or_init(|| {
            Pubkey::from_str(ATA_PROGRAM).expect("hard-coded ATA_PROGRAM is valid")
        })
    }

    pub fn system_program() -> Pubkey {
        solana_system_interface::program::id()
    }

    pub fn jito_tip_accounts() -> [Pubkey; 8] {
        static TIPS: OnceLock<[Pubkey; 8]> = OnceLock::new();
        *TIPS.get_or_init(|| {
            let mut arr = [Pubkey::default(); 8];
            for (i, s) in JITO_TIP_ACCOUNTS.iter().enumerate() {
                arr[i] = Pubkey::from_str(s)
                    .unwrap_or_else(|_| panic!("Invalid Jito tip account {i}: {s}"));
            }
            arr
        })
    }

    /// Eagerly initialize all hard-coded program address constants at startup.
    ///
    /// Each getter uses a OnceLock and panics on parse failure, so this function
    /// triggers initialization early (before any funds-touching logic) to surface
    /// any deployment mistakes immediately. It does not return a `Result` because
    /// the only failure mode is a panic from a malformed hard-coded constant —
    /// a programming error, not a runtime error.
    pub fn validate_constants() {
        // Trigger OnceLock initialization.  Each call panics if the hard-coded
        // address string is invalid.  These are const strings so parse failure
        // means a broken binary, not a recoverable runtime condition.
        let _ = wsol_mint();
        let _ = token_program();
        let _ = ata_program();
        let _ = jito_tip_accounts();
    }
}

// ── CLI args ─────────────────────────────────────────────────────────
// NOTE: Debug intentionally omitted — FEE_PAYER_KEYPAIR_BASE58 must not
// appear in logs or panic messages.

#[derive(Parser, Clone)]
#[command(name = "sol-arb-bot", about = "Solana flash-loan arbitrage bot")]
pub struct CliArgs {
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: String,

    #[arg(long, env = "GRPC_ENDPOINT", default_value = "")]
    pub grpc_endpoint: String,

    #[arg(long, env = "GRPC_X_TOKEN", default_value = "")]
    pub grpc_x_token: String,

    #[arg(long, env = "FALLBACK_RPC_URL", default_value = "https://api.mainnet-beta.solana.com")]
    pub fallback_rpc_url: String,

    #[arg(long, env = "FEE_PAYER_KEYPAIR_BASE58")]
    pub fee_payer_keypair_base58: String,

    #[arg(long, env = "MIN_PROFIT_SOL", default_value = "0.01")]
    pub min_profit_sol: f64,

    #[arg(long, env = "MAX_LOAN_SOL", default_value = "50.0")]
    pub max_loan_sol: f64,

    #[arg(long, env = "SLIPPAGE_BPS", default_value = "100")]
    pub slippage_bps: u16,

    #[arg(long, env = "JUPITER_API_URL", default_value = "http://127.0.0.1:8080")]
    pub jupiter_api_url: String,

    #[arg(long, env = "COMPUTE_UNIT_LIMIT", default_value = "400000")]
    pub compute_unit_limit: u32,

    #[arg(long, env = "PRIORITY_FEE_MICRO_LAMPORTS", default_value = "50000")]
    pub priority_fee_micro_lamports: u64,

    #[arg(long, env = "MAX_TX_RETRIES", default_value = "2")]
    pub max_tx_retries: u8,

    #[arg(long, env = "SCANNER_MAX_CONCURRENCY", default_value = "32")]
    pub scanner_max_concurrency: usize,

    #[arg(long, env = "JITO_ENABLED", default_value = "true")]
    pub jito_enabled: bool,

    #[arg(long, env = "JITO_BLOCK_ENGINE_URL", default_value = "https://mainnet.block-engine.jito.wtf")]
    pub _jito_block_engine_url: String,

    /// Floor tip in lamports — used when dynamic tip is zero (very small profits).
    #[arg(long, env = "JITO_TIP_FLOOR_LAMPORTS", default_value = "10000")]
    pub jito_tip_floor_lamports: u64,

    /// Ceiling tip in lamports — hard cap regardless of profit size.
    /// Default 0.1 SOL = 100_000_000 lamports.
    #[arg(long, env = "JITO_TIP_MAX_LAMPORTS", default_value = "100000000")]
    pub jito_tip_max_lamports: u64,

    /// Fraction of expected profit to bid as Jito tip (0.0–1.0).
    /// Industry standard is 0.50 (50%). Higher = better landing odds, lower net.
    #[arg(long, env = "JITO_TIP_PROFIT_FRACTION", default_value = "0.50")]
    pub jito_tip_profit_fraction: f64,

    /// Minimum fee-payer balance in lamports before execution is refused.
    /// Raised from the original 0.01 SOL to 0.5 SOL to cover worst-case
    /// tips (up to 0.1 SOL) + priority fees + base fees with headroom.
    #[arg(long, env = "MIN_BALANCE_LAMPORTS", default_value = "500000000")]
    pub min_balance_lamports: u64,

    /// Maximum slot age of an opportunity before the executor drops it without
    /// making any RPC calls. At ~400ms/slot, 20 slots ≈ 8 seconds.
    /// Opportunities older than this have quotes that are certainly stale.
    #[arg(long, env = "MAX_OPPORTUNITY_AGE_SLOTS", default_value = "20")]
    pub max_opportunity_age_slots: u64,

    /// Disable the ratatui TUI. When set, structured logs are written to stderr.
    /// Useful for CI, Docker, or systemd deployments without a TTY.
    #[arg(long, env = "NO_TUI", default_value = "false")]
    pub _no_tui: bool,

    // ── Tatum gRPC provider ──────────────────────────────────────────────
    //
    // When TATUM_API_KEY is set the bot routes its Yellowstone gRPC stream
    // through Tatum's managed gateway instead of the custom GRPC_ENDPOINT.
    // TATUM_API_KEY contains a secret — it is intentionally omitted from
    // Debug output (AppConfig does not derive Debug).

    /// Tatum API key.  When non-empty, the Tatum Solana gRPC gateway is used
    /// as the streaming provider and TATUM_GRPC_ENDPOINT is the target URI.
    /// Contains a secret — never log this field.
    #[arg(long, env = "TATUM_API_KEY", default_value = "")]
    pub tatum_api_key: String,

    /// Override the Tatum gRPC endpoint URI.
    /// Defaults to `https://solana-mainnet.gateway.tatum.io` when empty.
    /// Only used when TATUM_API_KEY is set.
    #[arg(
        long,
        env = "TATUM_GRPC_ENDPOINT",
        default_value = "https://solana-mainnet.gateway.tatum.io"
    )]
    pub tatum_grpc_endpoint: String,
}

// ── Resolved, validated config ───────────────────────────────────────

/// Runtime-validated application configuration.
///
/// # Security
///
/// This struct intentionally does NOT implement Debug. The `fee_payer` field
/// contains the private key of the hot wallet; it must never appear in logs,
/// tracing spans, panic output, or error messages. Use `fee_payer_pubkey`
/// (the public key only) for logging purposes.
pub struct AppConfig {
    pub rpc_url: String,
    pub fallback_rpc_url: String,
    pub grpc_endpoint: String,
    /// gRPC auth token. May be empty. Contains a secret; do not log.
    pub grpc_x_token: String,
    /// Fee-payer keypair. Private key — never log or clone unnecessarily.
    pub fee_payer: Keypair,
    pub min_profit_lamports: u64,
    pub max_loan_lamports: u64,
    pub slippage_bps: u16,
    pub jupiter_api_url: String,
    pub compute_unit_limit: u32,
    pub priority_fee_micro_lamports: u64,
    pub max_tx_retries: u8,
    pub scanner_max_concurrency: usize,
    pub jito_enabled: bool,
    pub _jito_block_engine_url: String,
    /// Floor tip — minimum even for tiny-profit arbs.
    pub jito_tip_floor_lamports: u64,
    /// Ceiling tip — hard cap.
    pub jito_tip_max_lamports: u64,
    /// Fraction of profit to bid as Jito tip (0.0–1.0).
    pub jito_tip_profit_fraction: f64,
    /// Minimum fee-payer balance before execution is refused (lamports).
    pub min_balance_lamports: u64,
    /// Drop opportunities older than this many slots without making any RPC calls.
    pub max_opportunity_age_slots: u64,
    pub _no_tui: bool,

    // ── Tatum gRPC provider ──────────────────────────────────────────────
    /// Tatum API key.  Non-empty when the Tatum provider is active.
    /// Contains a secret — never log this field.
    pub tatum_api_key: String,
    /// Tatum gRPC endpoint URI.
    pub tatum_grpc_endpoint: String,
}

impl AppConfig {
    pub fn from_cli(args: CliArgs) -> Result<Self> {
        validate_args(&args)?;
        programs::validate_constants();

        let key_bytes = bs58::decode(&args.fee_payer_keypair_base58)
            .into_vec()
            .context("FEE_PAYER_KEYPAIR_BASE58 is not valid base58")?;
        let fee_payer = Keypair::try_from(key_bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("FEE_PAYER_KEYPAIR_BASE58 invalid keypair: {e}"))?;

        let min_profit_lamports = sol_to_lamports(args.min_profit_sol, "MIN_PROFIT_SOL")?;
        let max_loan_lamports   = sol_to_lamports(args.max_loan_sol,   "MAX_LOAN_SOL")?;

        if max_loan_lamports < min_profit_lamports {
            anyhow::bail!(
                "MAX_LOAN_SOL ({}) must be >= MIN_PROFIT_SOL ({})",
                args.max_loan_sol, args.min_profit_sol
            );
        }

        Ok(Self {
            rpc_url:               args.rpc_url,
            fallback_rpc_url:      args.fallback_rpc_url,
            grpc_endpoint:         args.grpc_endpoint,
            grpc_x_token:          args.grpc_x_token,
            fee_payer,
            min_profit_lamports,
            max_loan_lamports,
            slippage_bps:          args.slippage_bps,
            jupiter_api_url:       args.jupiter_api_url.trim_end_matches('/').to_string(),
            compute_unit_limit:    args.compute_unit_limit,
            priority_fee_micro_lamports: args.priority_fee_micro_lamports,
            max_tx_retries:        args.max_tx_retries,
            scanner_max_concurrency: args.scanner_max_concurrency,
            jito_enabled:          args.jito_enabled,
            _jito_block_engine_url: args._jito_block_engine_url.trim_end_matches('/').to_string(),
            jito_tip_floor_lamports:   args.jito_tip_floor_lamports,
            jito_tip_max_lamports:     args.jito_tip_max_lamports,
            jito_tip_profit_fraction:  args.jito_tip_profit_fraction.clamp(0.0, 1.0),
            min_balance_lamports:  args.min_balance_lamports,
            max_opportunity_age_slots: args.max_opportunity_age_slots,
            _no_tui:                args._no_tui,
            tatum_api_key:         args.tatum_api_key,
            tatum_grpc_endpoint:   args.tatum_grpc_endpoint.trim_end_matches('/').to_string(),
        })
    }

    /// Compute the Jito tip for a given expected profit.
    ///
    /// Formula: clamp(profit × fraction, floor, max)
    ///
    /// The industry standard is 50% of expected profit. The floor ensures
    /// we always tip enough to be considered even on tiny-profit trades.
    /// The ceiling prevents runaway tips on unexpectedly large opportunities.
    pub fn dynamic_jito_tip(&self, expected_profit_lamports: u64) -> u64 {
        let dynamic = (expected_profit_lamports as f64 * self.jito_tip_profit_fraction) as u64;
        dynamic
            .max(self.jito_tip_floor_lamports)
            .min(self.jito_tip_max_lamports)
    }

    /// Estimated total tx cost used by the scanner to pre-filter unprofitable sizes.
    ///
    /// Uses the floor tip for conservative estimation — real cost at execution time
    /// is computed from the actual dynamic tip. This intentionally under-estimates
    /// so borderline opportunities still reach the executor, where a second check
    /// with the real dynamic tip is performed.
    pub fn estimated_tx_cost(&self) -> u64 {
        let priority_fee_lamports = (self.priority_fee_micro_lamports as u128
            * self.compute_unit_limit as u128
            / 1_000_000u128) as u64;

        // Base tx fee (5_000 lamports) + priority fee + floor tip.
        priority_fee_lamports + self.jito_tip_floor_lamports + 5_000
    }
}

fn validate_args(args: &CliArgs) -> Result<()> {
    if args.rpc_url.trim().is_empty() {
        anyhow::bail!("RPC_URL is required");
    }
    // GRPC_ENDPOINT is only required when TATUM_API_KEY is not set.
    // When Tatum is active it supplies its own endpoint.
    if args.tatum_api_key.trim().is_empty() && args.grpc_endpoint.trim().is_empty() {
        anyhow::bail!("Either GRPC_ENDPOINT or TATUM_API_KEY must be provided");
    }
    if args.fee_payer_keypair_base58.trim().is_empty() {
        anyhow::bail!("FEE_PAYER_KEYPAIR_BASE58 is required");
    }
    if !(1..=10_000).contains(&args.slippage_bps) {
        anyhow::bail!("SLIPPAGE_BPS must be between 1 and 10000");
    }
    if args.compute_unit_limit == 0 {
        anyhow::bail!("COMPUTE_UNIT_LIMIT must be > 0");
    }
    if args.max_tx_retries == 0 {
        anyhow::bail!("MAX_TX_RETRIES must be > 0");
    }
    if args.scanner_max_concurrency == 0 {
        anyhow::bail!("SCANNER_MAX_CONCURRENCY must be > 0");
    }
    if !args.jito_tip_profit_fraction.is_finite()
        || args.jito_tip_profit_fraction < 0.0
        || args.jito_tip_profit_fraction > 1.0
    {
        anyhow::bail!("JITO_TIP_PROFIT_FRACTION must be between 0.0 and 1.0");
    }
    if args.min_balance_lamports == 0 {
        anyhow::bail!("MIN_BALANCE_LAMPORTS must be > 0");
    }
    if args.max_opportunity_age_slots == 0 {
        anyhow::bail!("MAX_OPPORTUNITY_AGE_SLOTS must be > 0");
    }
    Ok(())
}

fn sol_to_lamports(sol: f64, label: &str) -> Result<u64> {
    if !sol.is_finite() || sol < 0.0 {
        anyhow::bail!("{label} must be a finite non-negative number");
    }
    let lamports = sol * 1_000_000_000.0;
    if lamports > u64::MAX as f64 {
        anyhow::bail!("{label} is too large");
    }
    Ok(lamports.round() as u64)
}
