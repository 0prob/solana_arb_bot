// src/flash_loan/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Flash loan provider selection and instruction building.
//
// Provider priority (lowest fee first):
//   JupiterLend (0 bps) → KaminoLend (0 bps) → Marginfi (0 bps) → Save (30 bps)
//
// Each build_* function returns a FlashLoanInstructions bundle that the
// executor splices into the arb transaction between the compute budget
// instructions and the Jupiter swap instructions.
// ═══════════════════════════════════════════════════════════════════════

use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::str::FromStr;
use tracing::debug;

use crate::config::programs;

// ── Provider enum ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashLoanProvider {
    JupiterLend,
    KaminoLend,
    Marginfi,
    SaveFinance,
}

/// Ordered by preference: lowest fee first, most liquid first.
/// The executor tries them in this order and returns the first success.
const WSOL_PROVIDERS: &[FlashLoanProvider] = &[
    FlashLoanProvider::JupiterLend,
    FlashLoanProvider::KaminoLend,
    FlashLoanProvider::Marginfi,
    FlashLoanProvider::SaveFinance,
];

impl FlashLoanProvider {
    /// Flash loan fee in basis points (charged on the borrow amount).
    pub fn fee_bps(self) -> u16 {
        match self {
            Self::JupiterLend => 0,
            Self::KaminoLend  => 0,
            Self::Marginfi    => 0,
            Self::SaveFinance => 30,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::JupiterLend => "jupiter_lend",
            Self::KaminoLend  => "kamino_lend",
            Self::Marginfi    => "marginfi",
            Self::SaveFinance => "save_finance",
        }
    }
}

/// Return the ordered candidate providers for borrowing `mint`.
/// Currently only WSOL is supported. Returns an empty slice for any other mint
/// so the caller can bail early with a clear error.
pub fn candidate_providers_for_borrow_mint(mint: &Pubkey) -> &'static [FlashLoanProvider] {
    let wsol = programs::wsol_mint();
    if *mint == wsol { WSOL_PROVIDERS } else { &[] }
}

/// Convenience: return the lowest fee_bps across all providers for `mint`.
pub fn best_fee_bps_for_borrow_mint(mint: &Pubkey) -> Option<u16> {
    candidate_providers_for_borrow_mint(mint)
        .iter()
        .map(|p| p.fee_bps())
        .min()
}

// ── Instruction bundle ───────────────────────────────────────────────

pub struct FlashLoanInstructions {
    /// Instruction that borrows `amount_lamports` from the provider.
    pub borrow_ix: Instruction,
    /// Instruction that repays the loan (principal + fee).
    pub repay_ix: Instruction,
    /// Fee in basis points (baked into the repay amount for providers that charge).
    pub fee_bps: u16,
    /// Optional instructions to run before borrow (e.g. create WSOL ATA).
    pub setup_ixs: Vec<Instruction>,
    /// If the provider's repay instruction embeds the index of the borrow
    /// instruction in its data, this is the byte offset at which to write it.
    /// None for providers that don't need this.
    pub repay_borrow_instruction_index_offset: Option<usize>,
}

pub fn build_flash_loan_instructions(
    provider: FlashLoanProvider,
    borrower: &Pubkey,
    amount_lamports: u64,
) -> Result<FlashLoanInstructions> {
    match provider {
        FlashLoanProvider::JupiterLend => build_jupiter_lend(borrower, amount_lamports),
        FlashLoanProvider::KaminoLend  => build_kamino_lend(borrower, amount_lamports),
        FlashLoanProvider::Marginfi    => build_marginfi(borrower, amount_lamports),
        FlashLoanProvider::SaveFinance => build_save(borrower, amount_lamports),
    }
}

// ── JupiterLend ──────────────────────────────────────────────────────

fn build_jupiter_lend(borrower: &Pubkey, amount_lamports: u64) -> Result<FlashLoanInstructions> {
    let program_id    = Pubkey::from_str("JLend1r8oU2rFZBMCren6bDPSAp7oX9CKBoe9Gf5QMc")?;
    let wsol_mint     = programs::wsol_mint();
    let token_program = programs::token_program();

    let lending_market = Pubkey::from_str("BJK9WSeU6bCWrTWCDVPuwQmST6dBvK5mk9FcwSqiULTm")?;
    let (lending_market_authority, _) =
        Pubkey::find_program_address(&[lending_market.as_ref()], &program_id);
    let sol_reserve = Pubkey::from_str("8LgGYc9LPhvNjZi2RAh1CF5JqxnF44QKjNvCcRTtwqeX")?;
    let (reserve_liquidity_supply, _) =
        Pubkey::find_program_address(&[b"liquidity_supply", sol_reserve.as_ref()], &program_id);
    let borrower_ata = associated_token_account(borrower, &wsol_mint)?;

    // Discriminator: sha256("global:flash_borrow_reserve_liquidity")[..8]
    let mut borrow_data = vec![0xc0, 0x20, 0x03, 0x84, 0xe8, 0x5f, 0x7c, 0x4b];
    borrow_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(lending_market,           false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new(sol_reserve,               false),
            AccountMeta::new(reserve_liquidity_supply,  false),
            AccountMeta::new(borrower_ata,              false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program,    false),
            AccountMeta::new(*borrower,                 true),
        ],
        data: borrow_data,
    };

    // Discriminator: sha256("global:flash_repay_reserve_liquidity")[..8]
    let mut repay_data = vec![0x87, 0xf7, 0x63, 0xc7, 0xb1, 0x4d, 0x98, 0x2e];
    repay_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(lending_market,           false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new(sol_reserve,               false),
            AccountMeta::new(reserve_liquidity_supply,  false),
            AccountMeta::new(borrower_ata,              false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program,    false),
            AccountMeta::new(*borrower,                 true),
        ],
        data: repay_data,
    };

    debug!(provider = "jupiter_lend", amount = amount_lamports, "Built flash loan pair");
    Ok(FlashLoanInstructions {
        borrow_ix,
        repay_ix,
        fee_bps: 0,
        setup_ixs: vec![create_ata_idempotent_ix(borrower)?],
        repay_borrow_instruction_index_offset: None,
    })
}

// ── KaminoLend ───────────────────────────────────────────────────────

fn build_kamino_lend(borrower: &Pubkey, amount_lamports: u64) -> Result<FlashLoanInstructions> {
    let program_id    = Pubkey::from_str("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD")?;
    let wsol_mint     = programs::wsol_mint();
    let token_program = programs::token_program();

    let lending_market = Pubkey::from_str("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF")?;
    let (lending_market_authority, _) =
        Pubkey::find_program_address(&[b"lma", lending_market.as_ref()], &program_id);
    let sol_reserve = Pubkey::from_str("d4A2prbA2nCUfGSbeDtBXNKkMfBLkNqjYhW95c7N3R1")?;
    let (reserve_liquidity_supply, _) = Pubkey::find_program_address(
        &[b"reserve_liq_supply", lending_market.as_ref(), sol_reserve.as_ref()],
        &program_id,
    );
    let borrower_ata = associated_token_account(borrower, &wsol_mint)?;

    // ix discriminator 19 = FlashBorrowReserveLiquidity
    let mut borrow_data = vec![19u8];
    borrow_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(*borrower,                 true),
            AccountMeta::new(borrower_ata,              false),
            AccountMeta::new(sol_reserve,               false),
            AccountMeta::new(reserve_liquidity_supply,  false),
            AccountMeta::new_readonly(lending_market,           false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program,    false),
        ],
        data: borrow_data,
    };

    // ix discriminator 20 = FlashRepayReserveLiquidity
    // Kamino v2 repay encodes: [disc(1)] [amount(8)] [borrow_ix_index(1)]
    // The borrow_ix_index is the index of the FlashBorrow instruction in the
    // transaction's instruction list — patched by the executor after layout is known.
    let mut repay_data = vec![20u8];
    repay_data.extend_from_slice(&amount_lamports.to_le_bytes());
    repay_data.push(0u8); // placeholder — patched at offset 9

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(*borrower,                 true),
            AccountMeta::new(borrower_ata,              false),
            AccountMeta::new(sol_reserve,               false),
            AccountMeta::new(reserve_liquidity_supply,  false),
            AccountMeta::new_readonly(lending_market,           false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program,    false),
        ],
        data: repay_data,
    };

    debug!(provider = "kamino_lend", amount = amount_lamports, "Built flash loan pair");
    Ok(FlashLoanInstructions {
        borrow_ix,
        repay_ix,
        fee_bps: 0,
        setup_ixs: vec![create_ata_idempotent_ix(borrower)?],
        // data layout: [disc(1)] [amount(8)] [borrow_ix_index(1)] → offset 9
        repay_borrow_instruction_index_offset: Some(9),
    })
}

// ── Marginfi ─────────────────────────────────────────────────────────
//
// MarginFi flash loans use a two-part pattern:
//   1. LendingAccountBorrow with flash_loan=true — pulls tokens from vault,
//      records a sysvar::instructions introspection requirement so the runtime
//      enforces that a matching LendingAccountRepay appears later in the tx.
//   2. LendingAccountRepay — pushes tokens + fee back to vault.
//
// CRITICAL: The flash_loan boolean flag (byte 16, value 1) MUST be set on the
// borrow instruction. Without it, MarginFi treats this as a normal borrow that
// doesn't require atomic same-tx repayment. The bot would then emit a tx that
// borrows but doesn't enforce repayment — a real fund-loss vector if the tx
// succeeds through simulation but later fails or gets exploited.
//
// Data layout for LendingAccountBorrow:
//   [0..8]   discriminator (8 bytes)
//   [8..16]  amount: u64 (little-endian)
//   [16]     flash_loan: bool (1 = true, required for atomic flash loan)

fn build_marginfi(borrower: &Pubkey, amount_lamports: u64) -> Result<FlashLoanInstructions> {
    let program_id    = Pubkey::from_str("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA")?;
    let wsol_mint     = programs::wsol_mint();
    let token_program = programs::token_program();

    let marginfi_group = Pubkey::from_str("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFkMPCfVE1")?;
    let (marginfi_account, _) =
        Pubkey::find_program_address(&[marginfi_group.as_ref(), borrower.as_ref()], &program_id);
    let sol_bank = Pubkey::from_str("CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAoD2t8k3r")?;
    let (bank_liquidity_vault, _) =
        Pubkey::find_program_address(&[b"liquidity_vault", sol_bank.as_ref()], &program_id);
    let (bank_liquidity_vault_authority, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", sol_bank.as_ref()],
        &program_id,
    );
    let borrower_ata = associated_token_account(borrower, &wsol_mint)?;

    // Discriminator: sha256("global:lending_account_borrow")[..8]
    // Amount is a little-endian u64 starting at byte 8.
    // flash_loan bool is at byte 16 — MUST be 1 (true) for atomic flash enforcement.
    let mut borrow_data = vec![0x1b, 0x41, 0x5c, 0x52, 0xb1, 0x87, 0xa2, 0xf5];
    borrow_data.extend_from_slice(&amount_lamports.to_le_bytes()); // bytes 8..16
    borrow_data.push(1u8); // byte 16: flash_loan = true — enforces same-tx repay

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(marginfi_group,    false),
            AccountMeta::new(marginfi_account,           false),
            AccountMeta::new(*borrower,                  true),
            AccountMeta::new(sol_bank,                   false),
            AccountMeta::new(bank_liquidity_vault,       false),
            AccountMeta::new_readonly(bank_liquidity_vault_authority, false),
            AccountMeta::new(borrower_ata,               false),
            AccountMeta::new_readonly(token_program,     false),
            // sysvar::instructions is required when flash_loan=true so the runtime
            // can introspect sibling instructions to enforce the repay exists.
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data: borrow_data,
    };

    // Discriminator: sha256("global:lending_account_repay")[..8]
    // Amount is le_u64 at byte 8; repay_all flag (0 = use amount) at byte 16.
    let mut repay_data = vec![0xd3, 0xd4, 0x27, 0x3c, 0xaa, 0x8b, 0x09, 0x14];
    repay_data.extend_from_slice(&amount_lamports.to_le_bytes());
    repay_data.push(0u8); // repay_all = false → use the explicit amount

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(marginfi_group,    false),
            AccountMeta::new(marginfi_account,           false),
            AccountMeta::new(*borrower,                  true),
            AccountMeta::new(sol_bank,                   false),
            AccountMeta::new(bank_liquidity_vault,       false),
            AccountMeta::new(borrower_ata,               false),
            AccountMeta::new_readonly(token_program,     false),
        ],
        data: repay_data,
    };

    debug!(provider = "marginfi", amount = amount_lamports, "Built flash loan pair");
    Ok(FlashLoanInstructions {
        borrow_ix,
        repay_ix,
        fee_bps: 0,
        setup_ixs: vec![create_ata_idempotent_ix(borrower)?],
        repay_borrow_instruction_index_offset: None,
    })
}

// ── Save (Solend) ────────────────────────────────────────────────────

fn build_save(borrower: &Pubkey, amount_lamports: u64) -> Result<FlashLoanInstructions> {
    let program_id    = Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hFSaS1cA1MwnTXGcA4K")?;
    let wsol_mint     = programs::wsol_mint();
    let token_program = programs::token_program();

    let lending_market           = Pubkey::from_str("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY")?;
    let (lending_market_authority, _) =
        Pubkey::find_program_address(&[lending_market.as_ref()], &program_id);
    let sol_reserve              = Pubkey::from_str("8PbodeaosQP19SjYFx855UMqWxH2HynZLdBXmsrbac36")?;
    let reserve_liquidity_supply = Pubkey::from_str("8UviNr47S8eL6J3WfDxMRa3hvLta1VDJwNWqsDgtN3Cv")?;
    let fee_receiver             = Pubkey::from_str("5wo1tFpi4HaVKnemqaXexQKfNKGkaA2paxSg6LKfphig")?;
    let borrower_ata = associated_token_account(borrower, &wsol_mint)?;

    // ix 20 = FlashBorrowReserveLiquidity
    let mut borrow_data = vec![20u8];
    borrow_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(reserve_liquidity_supply,      false),
            AccountMeta::new(borrower_ata,                  false),
            AccountMeta::new(sol_reserve,                   false),
            AccountMeta::new_readonly(lending_market,               false),
            AccountMeta::new_readonly(lending_market_authority,     false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program,        false),
        ],
        data: borrow_data,
    };

    // Save charges a flat 30 bps on the borrow amount.
    // Repay amount = principal + ceil(principal × 30 / 10_000).
    let fee          = fee_ceil(amount_lamports, 30);
    let repay_amount = amount_lamports
        .checked_add(fee)
        .ok_or_else(|| anyhow::anyhow!("Save repay overflow: {} + {}", amount_lamports, fee))?;

    // ix 21 = FlashRepayReserveLiquidity
    let mut repay_data = vec![21u8];
    repay_data.extend_from_slice(&repay_amount.to_le_bytes());
    repay_data.push(0u8); // placeholder for borrow_ix_index — patched at offset 9

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(borrower_ata,                  false),
            AccountMeta::new(reserve_liquidity_supply,      false),
            AccountMeta::new(fee_receiver,                  false),
            AccountMeta::new(sol_reserve,                   false),
            AccountMeta::new_readonly(lending_market,               false),
            AccountMeta::new_readonly(token_program,        false),
            AccountMeta::new(*borrower,                     true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data: repay_data,
    };

    debug!(provider = "save_finance", amount = amount_lamports, fee, "Built flash loan pair");
    Ok(FlashLoanInstructions {
        borrow_ix,
        repay_ix,
        fee_bps: 30,
        setup_ixs: vec![create_ata_idempotent_ix(borrower)?],
        // data layout: [disc(1)] [amount(8)] [borrow_ix_index(1)] → offset 9
        repay_borrow_instruction_index_offset: Some(9),
    })
}

// ── Utilities ────────────────────────────────────────────────────────

/// Ceiling division: computes ⌈amount × bps / 10_000⌉
fn fee_ceil(amount: u64, bps: u16) -> u64 {
    let n = amount as u128 * bps as u128;
    ((n + 9_999) / 10_000) as u64
}

fn associated_token_account(wallet: &Pubkey, mint: &Pubkey) -> Result<Pubkey> {
    let ata_program   = programs::ata_program();
    let token_program = programs::token_program();
    let (ata, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    Ok(ata)
}

/// Build an idempotent ATA creation instruction for the borrower's WSOL account.
/// Uses the ATA program's `create_idempotent` variant (data byte = 1), which
/// succeeds even if the account already exists.
fn create_ata_idempotent_ix(wallet: &Pubkey) -> Result<Instruction> {
    let wsol_mint     = programs::wsol_mint();
    let ata           = associated_token_account(wallet, &wsol_mint)?;
    let ata_program   = programs::ata_program();
    let token_program = programs::token_program();

    Ok(Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(*wallet,           true),
            AccountMeta::new(ata,               false),
            AccountMeta::new_readonly(*wallet,  false),
            AccountMeta::new_readonly(wsol_mint,              false),
            AccountMeta::new_readonly(programs::system_program(), false),
            AccountMeta::new_readonly(token_program,          false),
        ],
        data: vec![1], // create_idempotent discriminator
    })
}
