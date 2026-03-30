// src/logging/mod.rs
// ═══════════════════════════════════════════════════════════════════════
// Structured tracing with console output.
// Filter controlled by RUST_LOG env var (default: sb=info).
// ═══════════════════════════════════════════════════════════════════════

use tracing_subscriber::{fmt, EnvFilter};

pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("sb=info,yellowstone_grpc_client=warn"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(true)
        .init();
}
