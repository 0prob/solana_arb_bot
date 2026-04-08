# solana_arb_bot

Solana arbitrage bot focused on low-latency scanning and execution.

## What it does

- listens to Solana activity over Yellowstone gRPC
- scans for arbitrage opportunities
- executes through a fee-payer wallet
- supports headless mode or optional TUI mode

## Stack

- Rust
- Solana RPC
- Yellowstone gRPC
- Jupiter
- Jito
- ratatui
