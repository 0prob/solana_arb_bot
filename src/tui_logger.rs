use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{field::Visit, Subscriber};
use tracing_subscriber::Layer;
use crate::tui::TuiState;

pub struct TuiLoggerLayer {
    tx: mpsc::Sender<String>,
    state: Arc<Mutex<TuiState>>,
}

impl TuiLoggerLayer {
    pub fn new(tx: mpsc::Sender<String>, state: Arc<Mutex<TuiState>>) -> Self {
        Self { tx, state }
    }
}



struct StatsVisitor {
    message: String,
    profit_sol: Option<f64>,
}

impl Visit for StatsVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if field.name() == "profit_sol" {
            if let Ok(p) = format!("{:?}", value).parse::<f64>() {
                self.profit_sol = Some(p);
            }
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "profit_sol" {
            self.profit_sol = Some(value);
        }
    }
}

impl<S: Subscriber> Layer<S> for TuiLoggerLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = StatsVisitor { message: String::new(), profit_sol: None };
        event.record(&mut visitor);

        let level = *event.metadata().level();
        let target = event.metadata().target();
        
        let log_line = format!(
            "[{}] {} - {}",
            level,
            target,
            visitor.message.trim_matches('"')
        );

        // Update TUI state directly for stats
        if visitor.message.contains("Arbitrage opportunity found") {
            let mut s = self.state.lock().unwrap();
            s.opportunities_found += 1;
        } else if visitor.message.contains("Bundle submitted to Jito") {
            let mut s = self.state.lock().unwrap();
            s.bundles_submitted += 1;
            if let Some(p) = visitor.profit_sol {
                s.total_profit_sol += p;
            }
        }

        // Send to TUI channel
        let _ = self.tx.try_send(log_line);
    }
}
