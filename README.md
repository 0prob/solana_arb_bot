# Solana Flash-Loan Arbitrage Bot

A high-performance, zero-capital arbitrage bot for Solana. It listens to real-time liquidity pool creations via Yellowstone gRPC, evaluates multi-hop arbitrage opportunities via Jupiter V6, and executes atomic flash-loan transactions via Jito block engine.

## Architecture

The bot operates through a highly concurrent, modular pipeline:

1. **Listener (`src/listener/`)**: Subscribes to transaction streams via Yellowstone gRPC. Detects new pool creations across 25+ DEXes (Pump.fun, Raydium, Orca, Meteora, etc.) by inspecting both top-level and CPI inner instructions.
2. **Scanner (`src/scanner/`)**: Receives pool migration events and queries a self-hosted Jupiter V6 API for multi-hop arbitrage quotes across various loan sizes. Accurately accounts for slippage and estimated transaction costs to determine net profitability.
3. **Executor (`src/executor/`)**: Validates the opportunity, dynamically calculates priority fees and Jito tips, and constructs an atomic transaction. The transaction wraps the Jupiter swaps within a flash loan (JupiterLend, Kamino, Marginfi, or Save) and simulates the execution before submission.
4. **Jito Submission (`src/jito/`)**: Fans out the encoded bundle to all five Jito regional block engines simultaneously, minimizing latency to the current slot leader.

## Key Features & Security

- **Zero-Capital Operations**: Utilizes flash loans to execute arbitrage without requiring upfront capital. The bot automatically selects the optimal flash loan provider.
- **Strict Profitability Guarantees**: Uses the `other_amount_threshold` (worst-case output after slippage) to calculate expected profit and structure swap instructions, preventing "insufficient funds" failures during execution.
- **Dynamic Priority Fees & Tips**: Fetches recent prioritization fees and dynamically calculates Jito tips based on expected profit, ensuring competitive inclusion without overpaying.
- **Transaction Simulation**: Every transaction is simulated against the RPC node before submission to avoid paying fees for failed transactions.
- **Circuit Breakers**: Built-in safety mechanisms track consecutive failures and enforce cooldowns to protect the fee payer balance.
- **TUI Dashboard**: A `ratatui`-based terminal interface provides real-time observability into pipeline metrics, win rates, and execution logs.

## Configuration

Configuration is managed via environment variables (or a `.env` file). See `.env.example` for a complete list of options.

### Required Variables
- `RPC_URL`: HTTP endpoint for the Solana RPC node.
- `FEE_PAYER_KEYPAIR_BASE58`: Base58-encoded private key for the fee payer wallet.
- `GRPC_ENDPOINT` or `TATUM_API_KEY`: Yellowstone gRPC endpoint or Tatum API key for transaction streaming.

### Tuning Parameters
- `SLIPPAGE_BPS`: Maximum allowed slippage in basis points (e.g., `50` for 0.5%).
- `MIN_PROFIT_SOL`: Minimum expected net profit to trigger execution.
- `MAX_LOAN_SOL`: Maximum flash loan size to request.
- `JITO_TIP_PROFIT_FRACTION`: Fraction of expected profit to use as the Jito tip (e.g., `0.5` for 50%).

## Build & Run

Ensure you have Rust installed, then build the project:

```bash
cargo build --release
```

Run the bot:

```bash
cargo run --release
```

To run in headless mode (without the TUI, useful for systemd or Docker deployments):

```bash
cargo run --release -- --no-tui
```

## Disclaimer

This software is for educational and research purposes only. Arbitrage and MEV on Solana carry significant risks, including smart contract vulnerabilities, volatile network conditions, and financial loss. Use at your own risk.
