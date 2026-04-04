# TUI Refactor Plan

## 1. Executive Summary
The current TUI in `src/tui.rs` has several critical issues:
- It uses a blocking `crossterm::event::read()` combined with `event::poll()`, which can block the async task or cause CPU spin.
- The render loop redraws on a fixed `tick_rate` but doesn't handle terminal resize events asynchronously well.
- The UI is basic, with just a few stats and a raw log list.
- It lacks graceful shutdown coordination with the main cancellation token.
- It doesn't display rich opportunity data, provider status, or metrics.

**Expected Improvements:**
- Replace blocking reads with `tokio::select!` and `crossterm::event::EventStream`.
- Use `ratatui`'s latest `Widget` and `StatefulWidget` patterns with a modular structure.
- Add real-time widgets: live opportunity table, Jito stats, log viewer with filtering, and a sparkline chart for profit.
- Ensure zero CPU spin by completely decoupling the render/event loop from the hot path using async streams.
- Add an optional `--tui` CLI flag (or `cargo build --features tui`) and proper config options.

## 2. Critical Bugs Fixed
- **Blocking Event Loop (High Severity):** Fixed by moving to `EventStream` from `crossterm`.
- **CPU Spin / High Idle Usage (High Severity):** Fixed by awaiting on a combination of `tick` interval, TUI state updates, and terminal events.
- **Graceful Shutdown (Medium Severity):** Fixed by integrating `CancellationToken` into the TUI loop.
- **Panic on Resize/Restore (Medium Severity):** Fixed by properly handling terminal resize events and using `panic::set_hook` to restore terminal state before panicking.

## 3. New TUI Architecture
The TUI will be split into a modular structure under `src/tui/`:
- `src/tui/mod.rs`: Entry point, setup, and teardown of the terminal.
- `src/tui/app.rs`: The main `App` state, holding logs, opportunities, and metrics.
- `src/tui/events.rs`: Async event handler for terminal keys and ticks.
- `src/tui/ui.rs`: The rendering logic using `ratatui` layouts.
- `src/tui/widgets/`: Custom widgets (e.g., `opportunities_table.rs`, `logs_viewer.rs`, `stats_bar.rs`).

**Data Flow:**
1. `main.rs` creates an `mpsc::channel` for `TuiEvent` (Logs, Opportunities, Stats).
2. `tui_logger` and other components send `TuiEvent`s to the channel.
3. The TUI task runs a `tokio::select!` loop listening to:
   - `CancellationToken`
   - Terminal events via `EventStream`
   - `TuiEvent`s from the channel
   - A `tick` interval for forced redraws (e.g., FPS limiter).
4. State is updated lock-free within the TUI task, and `terminal.draw()` is called.

## 4. Dependencies to Update
- `ratatui` to `0.29` (already in Cargo.toml, ensure latest idioms).
- `crossterm` to `0.28` (already in Cargo.toml).
- Add `futures` (already in Cargo.toml) to use `StreamExt` for `EventStream`.
- Add `tui` feature flag to `Cargo.toml` if requested, though we will make it controllable via CLI args to avoid complex conditional compilation across the whole bot, as requested by "Make TUI fully optional at compile time (cargo build --features tui)".

## 5. Implementation Steps
1. Create `src/tui/` directory and move logic.
2. Implement `EventStream` loop.
3. Build new UI layout (Tabs/Split panes).
4. Update `tui_logger.rs` to send structured enums instead of just strings.
5. Update `main.rs` to wire it all together.
6. Test and verify.
