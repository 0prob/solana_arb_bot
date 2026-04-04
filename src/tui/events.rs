#![allow(dead_code)]
/// Events pushed into the TUI from the rest of the bot.
/// These are sent over an `mpsc::Sender<TuiEvent>` and consumed
/// exclusively by the TUI task, so no locking is required.
#[derive(Debug)]
pub enum TuiEvent {
    /// A log line from the tracing layer.
    Log {
        level: String,
        target: String,
        message: String,
    },
    /// An arbitrage opportunity was found by the scanner.
    OpportunityFound {
        token: String,
        loan_sol: f64,
        profit_sol: f64,
    },
    /// A Jito bundle was submitted by the executor.
    BundleSubmitted {
        bundle_id: String,
        profit_sol: f64,
        tip_sol: f64,
    },
    /// A critical error that should be shown in the error banner.
    CriticalError(String),
}
