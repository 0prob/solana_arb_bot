//! # flash_loan
//!
//! Builds MarginFi V2 flash-loan instruction pairs.
//!
//! ## Protocol overview
//!
//! MarginFi V2 flash loans work via instruction introspection:
//!
//! 1. `lending_account_start_flashloan` — marks the MarginFi account as IN_FLASHLOAN.
//!    Requires the index of the matching `end_flashloan` instruction as an argument so
//!    the program can verify the end instruction is present in the same transaction.
//! 2. Arbitrary borrow/swap instructions in between.
//! 3. `lending_account_end_flashloan` — unsets the flag and runs a full health check.
//!    Any borrow that was taken during the flash loan must be repaid before this runs.
//!
//! ## Account layout (from IDL v0.1.0)
//!
//! ### `lending_account_start_flashloan`
//! | # | Account              | Writable | Signer |
//! |---|----------------------|----------|--------|
//! | 0 | marginfi_account     | yes      | no     |
//! | 1 | signer (fee payer)   | no       | yes    |
//! | 2 | ixs_sysvar           | no       | no     |
//!
//! ### `lending_account_end_flashloan`
//! | # | Account              | Writable | Signer |
//! |---|----------------------|----------|--------|
//! | 0 | marginfi_account     | yes      | no     |
//! | 1 | signer (fee payer)   | no       | yes    |
//!
//! ## Discriminators (Anchor sha256 hash, first 8 bytes)
//!
//! | Instruction                          | Bytes (decimal)                    |
//! |--------------------------------------|------------------------------------|
//! | `lending_account_start_flashloan`    | [14, 131, 33, 220, 81, 186, 180, 107] |
//! | `lending_account_end_flashloan`      | [105, 124, 201, 106, 153, 2, 8, 156] |
//!
//! ## Important notes
//!
//! - The `start_flashloan` instruction must pass the **0-based index** of the
//!   `end_flashloan` instruction within the transaction's instruction list.
//!   This index is computed by the caller (executor) after all instructions are assembled.
//! - The flash loan does NOT directly move tokens.  Actual token movement is done by
//!   Jupiter swap instructions that reference the borrower's WSOL ATA.
//! - The WSOL ATA must exist before the transaction executes.  The `setup_ixs` returned
//!   here include an idempotent `create_associated_token_account_idempotent` instruction.

use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use crate::config::programs;

/// Discriminator for `lending_account_start_flashloan` (Anchor sha256 hash).
const START_FLASHLOAN_DISCRIMINATOR: [u8; 8] = [14, 131, 33, 220, 81, 186, 180, 107];

/// Discriminator for `lending_account_end_flashloan` (Anchor sha256 hash).
const END_FLASHLOAN_DISCRIMINATOR: [u8; 8] = [105, 124, 201, 106, 153, 2, 8, 156];

/// MarginFi V2 mainnet program ID.
const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYktwafCSNDh8nX1H6A21R5X";

/// Well-known MarginFi V2 mainnet group (the primary production group).
const MARGINFI_GROUP: &str = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8";

/// Well-known SOL bank in the primary MarginFi V2 group.
const MARGINFI_SOL_BANK: &str = "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh";

pub struct FlashLoanInstructions {
    /// Instruction to start the flash loan (must be first in the transaction).
    pub start_ix: Instruction,
    /// Instruction to end the flash loan (must be last in the transaction).
    /// The caller must patch `start_ix.data[8..16]` with the little-endian u64
    /// index of this instruction once the full instruction list is assembled.
    pub end_ix: Instruction,
    /// Setup instructions to run before `start_ix` (e.g., create WSOL ATA).
    pub setup_ixs: Vec<Instruction>,
}

/// Build MarginFi V2 flash-loan start/end instruction pair.
///
/// The caller is responsible for:
/// 1. Placing `setup_ixs` before `start_ix`.
/// 2. Placing `end_ix` as the last instruction in the transaction.
/// 3. Patching `start_ix.data[8..16]` with the little-endian u64 index of `end_ix`.
///
/// `amount_lamports` is informational only — actual token movement is handled by
/// the Jupiter swap instructions that reference the borrower's WSOL ATA.
pub fn build_flash_loan_instructions(
    borrower: &Pubkey,
    _amount_lamports: u64,
) -> Result<FlashLoanInstructions> {
    let program_id: Pubkey = MARGINFI_PROGRAM_ID.parse()?;
    let marginfi_group: Pubkey = MARGINFI_GROUP.parse()?;
    let marginfi_sol_bank: Pubkey = MARGINFI_SOL_BANK.parse()?;
    let wsol_mint = programs::wsol_mint();
    let token_program = programs::token_program();
    let ata_program = programs::ata_program();
    let system_program: Pubkey = "11111111111111111111111111111111".parse()?;

    // Derive the borrower's MarginFi account PDA.
    // MarginFi V2 uses a PDA seeded with [b"marginfi_account", group, authority].
    let (marginfi_account, _) = Pubkey::find_program_address(
        &[b"marginfi_account", marginfi_group.as_ref(), borrower.as_ref()],
        &program_id,
    );

    // Derive the borrower's WSOL ATA.
    let borrower_wsol_ata = Pubkey::find_program_address(
        &[borrower.as_ref(), token_program.as_ref(), wsol_mint.as_ref()],
        &ata_program,
    ).0;

    // ── start_flashloan instruction ───────────────────────────────────────
    // Data: discriminator (8 bytes) + end_index (u64 LE, 8 bytes).
    // The end_index placeholder (0) MUST be patched by the executor once the
    // full instruction list is assembled and the end_ix position is known.
    let mut start_data = START_FLASHLOAN_DISCRIMINATOR.to_vec();
    start_data.extend_from_slice(&0u64.to_le_bytes()); // placeholder; patch before signing

    let start_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data: start_data,
    };

    // ── end_flashloan instruction ─────────────────────────────────────────
    // Data: discriminator only (8 bytes). No args.
    // Remaining accounts: the SOL bank must be passed as a remaining account
    // so MarginFi can verify the borrow was repaid.
    let end_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(*borrower, true),
            // Remaining accounts: bank used for the flash-loan borrow.
            AccountMeta::new(marginfi_sol_bank, false),
        ],
        data: END_FLASHLOAN_DISCRIMINATOR.to_vec(),
    };

    // ── Setup: create WSOL ATA idempotently ──────────────────────────────
    // Uses the idempotent variant (instruction discriminant = 1) so it is safe
    // to include even if the ATA already exists.
    let create_wsol_ata_ix = Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(*borrower, true),          // funder
            AccountMeta::new(borrower_wsol_ata, false), // ATA to create
            AccountMeta::new_readonly(*borrower, false), // wallet owner
            AccountMeta::new_readonly(wsol_mint, false), // mint
            AccountMeta::new_readonly(system_program, false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1], // idempotent create
    };

    Ok(FlashLoanInstructions {
        start_ix,
        end_ix,
        setup_ixs: vec![create_wsol_ata_ix],
    })
}
