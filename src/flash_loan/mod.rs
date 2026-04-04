use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::str::FromStr;
use crate::config::programs;

pub struct FlashLoanInstructions {
    pub borrow_ix: Instruction,
    pub repay_ix: Instruction,
    pub setup_ixs: Vec<Instruction>,
}

pub fn build_flash_loan_instructions(
    borrower: &Pubkey,
    amount_lamports: u64,
) -> Result<FlashLoanInstructions> {
    let program_id = Pubkey::from_str("JLend1r8oU2rFZBMCren6bDPSAp7oX9CKBoe9Gf5QMc")?;
    let wsol_mint = programs::wsol_mint();
    let token_program = programs::token_program();
    let lending_market = Pubkey::from_str("BJK9WSeU6bCWrTWCDVPuwQmST6dBvK5mk9FcwSqiULTm")?;
    let (lending_market_authority, _) = Pubkey::find_program_address(&[lending_market.as_ref()], &program_id);
    let sol_reserve = Pubkey::from_str("8LgGYc9LPhvNjZi2RAh1CF5JqxnF44QKjNvCcRTtwqeX")?;
    let (reserve_liquidity_supply, _) = Pubkey::find_program_address(&[b"liquidity_supply", sol_reserve.as_ref()], &program_id);
    let borrower_ata = Pubkey::find_program_address(&[borrower.as_ref(), token_program.as_ref(), wsol_mint.as_ref()], &programs::ata_program()).0;

    let mut borrow_data = vec![0xc0, 0x20, 0x03, 0x84, 0xe8, 0x5f, 0x7c, 0x4b];
    borrow_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new(sol_reserve, false),
            AccountMeta::new(reserve_liquidity_supply, false),
            AccountMeta::new(borrower_ata, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new(*borrower, true),
        ],
        data: borrow_data,
    };

    let mut repay_data = vec![0x87, 0xf7, 0x63, 0xc7, 0xb1, 0x4d, 0x98, 0x2e];
    repay_data.extend_from_slice(&amount_lamports.to_le_bytes());

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new(sol_reserve, false),
            AccountMeta::new(reserve_liquidity_supply, false),
            AccountMeta::new(borrower_ata, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new(*borrower, true),
        ],
        data: repay_data,
    };

    let setup_ixs = vec![Instruction {
        program_id: programs::ata_program(),
        accounts: vec![
            AccountMeta::new(*borrower, true),
            AccountMeta::new(borrower_ata, false),
            AccountMeta::new_readonly(*borrower, false),
            AccountMeta::new_readonly(wsol_mint, false),
            AccountMeta::new_readonly(Pubkey::from_str("11111111111111111111111111111111").unwrap(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1],
    }];

    Ok(FlashLoanInstructions { borrow_ix, repay_ix, setup_ixs })
}
