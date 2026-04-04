#![cfg(feature = "tui")]
use tokio::sync::mpsc;
use tracing::{field::Visit, Subscriber};
use tracing_subscriber::Layer;
use crate::tui::events::TuiEvent;

/// A `tracing` subscriber layer that forwards log events to the TUI via an
/// async channel.  It parses structured fields (`token`, `loan_sol`,
/// `profit_sol`, `tip_sol`, `bundle_id`) to emit rich `TuiEvent` variants
/// rather than raw strings, eliminating fragile string-matching in the TUI.
pub struct TuiLoggerLayer {
    tx: mpsc::Sender<TuiEvent>,
}

impl TuiLoggerLayer {
    pub fn new(tx: mpsc::Sender<TuiEvent>) -> Self {
        Self { tx }
    }
}

// ── Field visitor ─────────────────────────────────────────────────────────────

struct EventVisitor {
    message: String,
    token: Option<String>,
    loan_sol: Option<f64>,
    profit_sol: Option<f64>,
    tip_sol: Option<f64>,
    bundle_id: Option<String>,
}

impl EventVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            token: None,
            loan_sol: None,
            profit_sol: None,
            tip_sol: None,
            bundle_id: None,
        }
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}").trim_matches('"').to_string(),
            "token" => self.token = Some(format!("{value:?}").trim_matches('"').to_string()),
            "bundle_id" => {
                self.bundle_id = Some(format!("{value:?}").trim_matches('"').to_string())
            }
            "loan_sol" => {
                if let Ok(v) = format!("{value:?}").parse::<f64>() {
                    self.loan_sol = Some(v);
                }
            }
            "profit_sol" => {
                if let Ok(v) = format!("{value:?}").parse::<f64>() {
                    self.profit_sol = Some(v);
                }
            }
            "tip_sol" => {
                if let Ok(v) = format!("{value:?}").parse::<f64>() {
                    self.tip_sol = Some(v);
                }
            }
            _ => {}
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        match field.name() {
            "profit_sol" => self.profit_sol = Some(value),
            "tip_sol" => self.tip_sol = Some(value),
            "loan_sol" => self.loan_sol = Some(value),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "token" => self.token = Some(value.to_string()),
            "bundle_id" => self.bundle_id = Some(value.to_string()),
            _ => {}
        }
    }
}

// ── Layer impl ────────────────────────────────────────────────────────────────

impl<S: Subscriber> Layer<S> for TuiLoggerLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventVisitor::new();
        event.record(&mut visitor);

        let level = event.metadata().level().to_string();
        let target = event.metadata().target().to_string();

        // Emit structured events for known hot-path messages.
        if visitor.message.contains("Arbitrage opportunity found") {
            let _ = self.tx.try_send(TuiEvent::OpportunityFound {
                token: visitor.token.unwrap_or_default(),
                loan_sol: visitor.loan_sol.unwrap_or(0.0),
                profit_sol: visitor.profit_sol.unwrap_or(0.0),
            });
        } else if visitor.message.contains("Bundle submitted to Jito") {
            let _ = self.tx.try_send(TuiEvent::BundleSubmitted {
                bundle_id: visitor.bundle_id.unwrap_or_default(),
                profit_sol: visitor.profit_sol.unwrap_or(0.0),
                tip_sol: visitor.tip_sol.unwrap_or(0.0),
            });
        }

        // Always forward the raw log line.
        let _ = self.tx.try_send(TuiEvent::Log {
            level,
            target,
            message: visitor.message,
        });
    }
}
