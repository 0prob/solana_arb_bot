#![allow(dead_code)]
#![cfg(feature = "tui")]
use std::collections::VecDeque;
use std::time::Instant;

/// Maximum log lines retained in memory (mobile-safe: 100 instead of 500).
pub const MAX_LOGS: usize = 100;
/// Maximum opportunities retained (mobile-safe: 20 instead of 50).
pub const MAX_OPPORTUNITIES: usize = 20;
/// Maximum sparkline data-points (mobile-safe: 30 instead of 60).
pub const MAX_SPARKLINE: usize = 30;
/// Maximum bundle records retained.
pub const MAX_BUNDLES: usize = 20;

/// A single recorded arbitrage opportunity.
#[derive(Debug, Clone)]
pub struct OpportunityRecord {
    pub token: String,
    pub loan_sol: f64,
    pub profit_sol: f64,
    pub timestamp: Instant,
}

/// A single Jito bundle submission record.
#[derive(Debug, Clone)]
pub struct BundleRecord {
    pub bundle_id: String,
    pub profit_sol: f64,
    pub tip_sol: f64,
    pub timestamp: Instant,
}

/// Severity level for log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "ERROR" => Self::Error,
            "WARN" => Self::Warn,
            "DEBUG" => Self::Debug,
            "TRACE" => Self::Trace,
            _ => Self::Info,
        }
    }
}

/// A single log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub timestamp: Instant,
}

/// Which tab is currently active in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Dashboard,
    Opportunities,
    Logs,
    Help,
}

impl ActiveTab {
    pub fn next(self) -> Self {
        match self {
            Self::Dashboard => Self::Opportunities,
            Self::Opportunities => Self::Logs,
            Self::Logs => Self::Help,
            Self::Help => Self::Dashboard,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Dashboard => Self::Help,
            Self::Opportunities => Self::Dashboard,
            Self::Logs => Self::Opportunities,
            Self::Help => Self::Logs,
        }
    }
    pub fn titles() -> &'static [&'static str] {
        &["[1] Dashboard", "[2] Opportunities", "[3] Logs", "[4] Help"]
    }
    pub fn index(self) -> usize {
        match self {
            Self::Dashboard => 0,
            Self::Opportunities => 1,
            Self::Logs => 2,
            Self::Help => 3,
        }
    }
}

/// The full application state owned exclusively by the TUI task.
pub struct App {
    pub should_quit: bool,
    pub paused: bool,
    pub start_time: Instant,
    pub active_tab: ActiveTab,
    pub opportunities_found: u64,
    pub bundles_submitted: u64,
    pub total_profit_sol: f64,
    pub total_tip_sol: f64,
    pub render_time_us: u64,
    pub events_processed: u64,
    pub opportunities: VecDeque<OpportunityRecord>,
    pub bundles: VecDeque<BundleRecord>,
    pub logs: VecDeque<LogEntry>,
    pub profit_sparkline: VecDeque<u64>,
    pub log_filter: Option<String>,
    pub log_scroll: usize,
    pub opp_scroll: usize,
    pub error_banner: Option<(String, Instant)>,
    pub mouse_enabled: bool,
    pub compact: bool,
}

impl App {
    pub fn new(mouse_enabled: bool, compact: bool) -> Self {
        Self {
            should_quit: false,
            paused: false,
            start_time: Instant::now(),
            active_tab: ActiveTab::Dashboard,
            opportunities_found: 0,
            bundles_submitted: 0,
            total_profit_sol: 0.0,
            total_tip_sol: 0.0,
            render_time_us: 0,
            events_processed: 0,
            opportunities: VecDeque::with_capacity(MAX_OPPORTUNITIES),
            bundles: VecDeque::with_capacity(MAX_BUNDLES),
            logs: VecDeque::with_capacity(MAX_LOGS),
            profit_sparkline: VecDeque::with_capacity(MAX_SPARKLINE),
            log_filter: None,
            log_scroll: 0,
            opp_scroll: 0,
            error_banner: None,
            mouse_enabled,
            compact,
        }
    }

    pub fn add_opportunity(&mut self, token: String, loan_sol: f64, profit_sol: f64) {
        if self.paused { return; }
        self.opportunities_found += 1;
        let micro = (profit_sol * 1_000_000.0).round() as u64;
        if self.profit_sparkline.len() >= MAX_SPARKLINE {
            self.profit_sparkline.pop_front();
        }
        self.profit_sparkline.push_back(micro);
        if self.opportunities.len() >= MAX_OPPORTUNITIES {
            self.opportunities.pop_front();
        }
        self.opportunities.push_back(OpportunityRecord {
            token,
            loan_sol,
            profit_sol,
            timestamp: Instant::now(),
        });
    }

    pub fn add_bundle(&mut self, bundle_id: String, profit_sol: f64, tip_sol: f64) {
        self.bundles_submitted += 1;
        self.total_profit_sol += profit_sol;
        self.total_tip_sol += tip_sol;
        if self.bundles.len() >= MAX_BUNDLES {
            self.bundles.pop_front();
        }
        self.bundles.push_back(BundleRecord {
            bundle_id,
            profit_sol,
            tip_sol,
            timestamp: Instant::now(),
        });
    }

    pub fn add_log(&mut self, level: LogLevel, target: String, message: String) {
        if self.logs.len() >= MAX_LOGS {
            self.logs.pop_front();
            if self.log_scroll > 0 {
                self.log_scroll -= 1;
            }
        }
        self.logs.push_back(LogEntry {
            level,
            target,
            message,
            timestamp: Instant::now(),
        });
    }

    pub fn set_error(&mut self, msg: String) {
        self.error_banner = Some((msg, Instant::now()));
    }

    pub fn clear_error(&mut self) {
        self.error_banner = None;
    }

    /// Filtered log view — returns indices into `self.logs`.
    pub fn filtered_logs(&self) -> Vec<usize> {
        match &self.log_filter {
            None => (0..self.logs.len()).collect(),
            Some(f) => {
                let f_lower = f.to_ascii_lowercase();
                self.logs
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        e.message.to_ascii_lowercase().contains(&f_lower)
                            || e.target.to_ascii_lowercase().contains(&f_lower)
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        }
    }

    pub fn uptime_str(&self) -> String {
        let secs = self.start_time.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h:02}:{m:02}:{s:02}")
    }
}
