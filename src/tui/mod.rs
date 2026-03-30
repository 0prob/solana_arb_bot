// src/tui/mod.rs
// ═══════════════════════════════════════════════════════════════════════
//  Ratatui TUI Dashboard
//
//  spawn()      — full ratatui terminal dashboard (default)
//  spawn_null() — no-op stub for --no-tui / headless mode
//
//  Layout:
//  ┌──────────────── SOL-ARB-BOT ─────────────────────────────────────┐
//  │ uptime │ gRPC status │ net profit │ confirmed/submitted/win-rate │
//  ├─────────────────┬────────────────────────────────────────────────┤
//  │  Pipeline       │  Activity Log (scrolling, 500-entry ring buf)  │
//  │  counters &     │                                                │
//  │  win-rate gauge │                                                │
//  ├─────────────────┴────────────────────────────────────────────────┤
//  │  Confirmed Opportunities table (most recent 50)                  │
//  ├──────────────────────────────────────────────────────────────────┤
//  │  [q] quit  [↑/↓] scroll log  [PgUp/PgDn] page  [End] live      │
//  └──────────────────────────────────────────────────────────────────┘
// ═══════════════════════════════════════════════════════════════════════

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::{collections::VecDeque, io::Stdout, time::{Duration, Instant}};
use tokio_util::sync::CancellationToken;

// ── Public event type ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DashEvent {
    // Listener
    ListenerConnected    { endpoint: String },
    ListenerReconnecting { attempt: u32 },
    PoolDetected         { token: String, dex: String, slot: u64 },

    // Scanner
    ScannerEvaluating  { token: String },
    ScannerProfitable  { token: String, loan_sol: f64, profit_sol: f64 },
    ScannerUnprofitable { token: String },
    ScannerTimeout     { token: String },
    ScannerError       { token: String, msg: String },

    // Executor
    ExecutorSubmitting { token: String, loan_sol: f64 },
    ExecutorConfirmed {
        token:      String,
        signature:  String,
        profit_sol: f64,
        tip_sol:    f64,
        fee_sol:    f64,
        via_jito:   bool,
    },
    ExecutorFailed      { token: String, reason: String },
    ExecutorSimRejected { token: String },
    ExecutorStaleQuote  { token: String },
    CircuitBreakerTripped { consecutive_failures: u32 },

    // Metrics heartbeat — authoritative snapshot from atomic counters.
    MetricsTick {
        evaluated:         u64,
        profitable:        u64,
        submitted:         u64,
        confirmed:         u64,
        failed:            u64,
        sim_rejected:      u64,
        stale_rejected:    u64,
        net_profit_sol:    f64,
        bundles_sent:      u64,
        bundles_landed:    u64,
        bundles_timed_out: u64,
    },

    Shutdown,
}

// ── Internal dashboard state ─────────────────────────────────────────

#[derive(Default, Clone)]
struct Counters {
    pools_detected:    u64,
    evaluated:         u64,
    profitable:        u64,
    submitted:         u64,
    confirmed:         u64,
    failed:            u64,
    sim_rejected:      u64,
    stale_rejected:    u64,
    // Authoritative from MetricsTick (atomic snapshot).
    // NOT accumulated via ExecutorConfirmed — that would double-count.
    net_profit_sol:    f64,
    bundles_sent:      u64,
    bundles_landed:    u64,
    bundles_timed_out: u64,
}

#[derive(Clone)]
struct OppRecord {
    token:      String,
    profit_sol: f64,
    signature:  String,
    ts:         Instant,
}

#[derive(Clone)]
struct LogEntry {
    level:     LogLevel,
    subsystem: &'static str,
    msg:       String,
    ts:        Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum LogLevel { Info, Warn, Error, Success }

#[derive(Clone, PartialEq)]
enum ListenerStatus {
    Connecting,
    Connected(String),
    Reconnecting(u32),
}

struct DashState {
    counters:        Counters,
    log:             VecDeque<LogEntry>,
    opportunities:   VecDeque<OppRecord>,
    listener_status: ListenerStatus,
    scanner_active:  u32,
    start:           Instant,
}

const LOG_CAPACITY: usize  = 500;
const OPP_CAPACITY: usize  = 50;

impl DashState {
    fn new() -> Self {
        Self {
            counters:        Counters::default(),
            log:             VecDeque::with_capacity(LOG_CAPACITY),
            opportunities:   VecDeque::with_capacity(OPP_CAPACITY),
            listener_status: ListenerStatus::Connecting,
            scanner_active:  0,
            start:           Instant::now(),
        }
    }

    fn push_log(&mut self, level: LogLevel, subsystem: &'static str, msg: impl Into<String>) {
        if self.log.len() >= LOG_CAPACITY { self.log.pop_front(); }
        self.log.push_back(LogEntry { level, subsystem, msg: msg.into(), ts: Instant::now() });
    }

    fn apply(&mut self, ev: DashEvent) {
        match ev {
            DashEvent::ListenerConnected { endpoint } => {
                self.listener_status = ListenerStatus::Connected(endpoint.clone());
                self.push_log(LogLevel::Success, "LISTENER",
                    format!("Connected → {}", shorten(&endpoint, 40)));
            }
            DashEvent::ListenerReconnecting { attempt } => {
                self.listener_status = ListenerStatus::Reconnecting(attempt);
                self.push_log(LogLevel::Warn, "LISTENER",
                    format!("Reconnecting (attempt {})", attempt));
            }
            DashEvent::PoolDetected { token, dex, slot } => {
                self.counters.pools_detected += 1;
                self.push_log(LogLevel::Info, "LISTENER",
                    format!("Pool  {} on {} slot={}", shorten(&token, 12), dex, slot));
            }
            DashEvent::ScannerEvaluating { .. } => {
                self.scanner_active = self.scanner_active.saturating_add(1);
            }
            DashEvent::ScannerProfitable { token, loan_sol, profit_sol } => {
                self.counters.profitable += 1;
                self.scanner_active = self.scanner_active.saturating_sub(1);
                self.push_log(LogLevel::Success, "SCANNER",
                    format!("PROFITABLE  {} loan={:.2} profit={:.6} SOL",
                        shorten(&token, 12), loan_sol, profit_sol));
            }
            DashEvent::ScannerUnprofitable { .. } => {
                self.scanner_active = self.scanner_active.saturating_sub(1);
            }
            DashEvent::ScannerTimeout { token } => {
                self.scanner_active = self.scanner_active.saturating_sub(1);
                self.push_log(LogLevel::Warn, "SCANNER",
                    format!("Timeout  {}", shorten(&token, 24)));
            }
            DashEvent::ScannerError { token, msg } => {
                self.scanner_active = self.scanner_active.saturating_sub(1);
                self.push_log(LogLevel::Error, "SCANNER",
                    format!("Error {} — {}", shorten(&token, 12), msg));
            }
            DashEvent::ExecutorSubmitting { token, loan_sol } => {
                self.counters.submitted += 1;
                self.push_log(LogLevel::Info, "EXECUTOR",
                    format!("Submitting  {} loan={:.2} SOL", shorten(&token, 12), loan_sol));
            }
            DashEvent::ExecutorConfirmed { token, signature, profit_sol, tip_sol: _, fee_sol: _, via_jito } => {
                // NOTE: do NOT accumulate net_profit_sol here.
                // MetricsTick carries the authoritative value from atomic counters.
                // Accumulating here AND in MetricsTick would double-count every trade.
                self.counters.confirmed += 1;
                let via = if via_jito { "Jito" } else { "RPC" };
                self.push_log(LogLevel::Success, "EXECUTOR",
                    format!("CONFIRMED via {}  sig={}  profit≈{:+.6} SOL",
                        via, shorten(&signature, 16), profit_sol));
                if self.opportunities.len() >= OPP_CAPACITY {
                    self.opportunities.pop_back();
                }
                self.opportunities.push_front(OppRecord {
                    token:      shorten(&token, 16),
                    profit_sol,
                    signature:  shorten(&signature, 22),
                    ts:         Instant::now(),
                });
            }
            DashEvent::ExecutorFailed { token, reason } => {
                self.counters.failed += 1;
                self.push_log(LogLevel::Error, "EXECUTOR",
                    format!("Failed  {} — {}", shorten(&token, 12), shorten(&reason, 36)));
            }
            DashEvent::ExecutorSimRejected { token } => {
                self.counters.sim_rejected += 1;
                self.push_log(LogLevel::Warn, "EXECUTOR",
                    format!("Sim rejected  {}", shorten(&token, 24)));
            }
            DashEvent::ExecutorStaleQuote { token } => {
                self.counters.stale_rejected += 1;
                self.push_log(LogLevel::Warn, "EXECUTOR",
                    format!("Stale quote  {}", shorten(&token, 24)));
            }
            DashEvent::CircuitBreakerTripped { consecutive_failures } => {
                self.push_log(LogLevel::Error, "SAFETY",
                    format!("CIRCUIT BREAKER  {} consecutive failures", consecutive_failures));
            }
            // MetricsTick is the single authoritative source for all cumulative counters.
            // It overwrites whatever local estimates were built from individual events.
            DashEvent::MetricsTick {
                evaluated, profitable, submitted, confirmed, failed,
                sim_rejected, stale_rejected, net_profit_sol,
                bundles_sent, bundles_landed, bundles_timed_out,
            } => {
                self.counters.evaluated         = evaluated;
                self.counters.profitable        = profitable;
                self.counters.submitted         = submitted;
                self.counters.confirmed         = confirmed;
                self.counters.failed            = failed;
                self.counters.sim_rejected      = sim_rejected;
                self.counters.stale_rejected    = stale_rejected;
                self.counters.net_profit_sol    = net_profit_sol;
                self.counters.bundles_sent      = bundles_sent;
                self.counters.bundles_landed    = bundles_landed;
                self.counters.bundles_timed_out = bundles_timed_out;
            }
            DashEvent::Shutdown => {
                self.push_log(LogLevel::Warn, "SYSTEM", "Shutdown signal received — draining…");
            }
        }
    }
}

// ── Public handle ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DashHandle {
    tx: tokio::sync::mpsc::UnboundedSender<DashEvent>,
}

impl DashHandle {
    /// Non-blocking fire-and-forget. Silently drops if TUI has exited.
    pub fn send(&self, ev: DashEvent) {
        let _ = self.tx.send(ev);
    }
}

/// Spawn the full ratatui dashboard thread. Returns a `DashHandle` for
/// all async subsystems. Pressing `q` / Ctrl-C cancels the token.
///
/// Gracefully degrades: if the terminal cannot be initialised (not a TTY,
/// insufficient permissions) the error is logged and `spawn_null()` is used
/// transparently so the bot continues to run.
pub fn spawn(cancel: CancellationToken) -> DashHandle {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DashEvent>();
    let handle = DashHandle { tx };

    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();

        // Graceful degradation: if we can't set up the terminal (e.g. not a TTY),
        // drain events silently so the rest of the bot keeps running.
        if enable_raw_mode().is_err() {
            while let Some(ev) = rx.blocking_recv() {
                if matches!(ev, DashEvent::Shutdown) { break; }
            }
            return;
        }

        if execute!(stdout, EnterAlternateScreen).is_err() {
            let _ = disable_raw_mode();
            while let Some(ev) = rx.blocking_recv() {
                if matches!(ev, DashEvent::Shutdown) { break; }
            }
            return;
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(_) => {
                let _ = disable_raw_mode();
                while let Some(ev) = rx.blocking_recv() {
                    if matches!(ev, DashEvent::Shutdown) { break; }
                }
                return;
            }
        };

        let mut state       = DashState::new();
        let mut log_scroll  = 0usize;
        let mut table_state = TableState::default();
        let tick            = Duration::from_millis(100);

        loop {
            let mut shutting_down = false;
            while let Ok(ev) = rx.try_recv() {
                if matches!(ev, DashEvent::Shutdown) { shutting_down = true; }
                state.apply(ev);
            }

            let _ = terminal.draw(|f| render(f, &state, log_scroll, &mut table_state));

            if shutting_down {
                std::thread::sleep(Duration::from_millis(600));
                cleanup(&mut terminal);
                return;
            }

            if event::poll(tick).unwrap_or(false) {
                if let Ok(Event::Key(k)) = event::read() {
                    match (k.code, k.modifiers) {
                        (KeyCode::Char('q'), _)
                        | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            cancel.cancel();
                            cleanup(&mut terminal);
                            return;
                        }
                        (KeyCode::Up, _)       => { log_scroll = log_scroll.saturating_add(1); }
                        (KeyCode::Down, _)     => { log_scroll = log_scroll.saturating_sub(1); }
                        (KeyCode::PageUp, _)   => { log_scroll = log_scroll.saturating_add(10); }
                        (KeyCode::PageDown, _) => { log_scroll = log_scroll.saturating_sub(10); }
                        (KeyCode::End, _)      => { log_scroll = 0; }
                        (KeyCode::Home, _)     => {
                            log_scroll = state.log.len().saturating_sub(1);
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    handle
}

/// Spawn a no-op TUI handle for headless / --no-tui mode.
/// All events are silently discarded in a background task.
pub fn spawn_null() -> DashHandle {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DashEvent>();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if matches!(ev, DashEvent::Shutdown) { break; }
        }
    });
    DashHandle { tx }
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

// ── Top-level render ─────────────────────────────────────────────────

fn render(f: &mut Frame, state: &DashState, log_scroll: usize, table_state: &mut TableState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header bar
            Constraint::Min(10),    // body (gauges + log)
            Constraint::Length(5),  // confirmed opps table
            Constraint::Length(1),  // footer keybindings
        ])
        .split(f.area());

    draw_header(f, state, root[0]);
    draw_body(f, state, log_scroll, root[1]);
    draw_opp_table(f, state, table_state, root[2]);
    draw_footer(f, root[3]);
}

// ── Header ───────────────────────────────────────────────────────────

fn draw_header(f: &mut Frame, state: &DashState, area: Rect) {
    let elapsed = state.start.elapsed().as_secs();
    let uptime  = format!("{:02}:{:02}:{:02}", elapsed / 3600, (elapsed % 3600) / 60, elapsed % 60);

    let grpc_span = match &state.listener_status {
        ListenerStatus::Connecting      => Span::styled("connecting…",            Style::default().fg(Color::Yellow)),
        ListenerStatus::Connected(_)    => Span::styled("connected ✓",            Style::default().fg(Color::Green)),
        ListenerStatus::Reconnecting(n) => Span::styled(format!("reconnect({})", n), Style::default().fg(Color::Red)),
    };

    let net_color = if state.counters.net_profit_sol >= 0.0 { Color::Green } else { Color::Red };
    let win = if state.counters.submitted > 0 {
        format!("{:.1}%", state.counters.confirmed as f64 / state.counters.submitted as f64 * 100.0)
    } else { "—".to_string() };

    let bundle_rate = if state.counters.bundles_sent > 0 {
        format!("{:.1}%",
            state.counters.bundles_landed as f64 / state.counters.bundles_sent as f64 * 100.0)
    } else { "—".to_string() };

    let dim = Style::default().fg(Color::DarkGray);
    let line = Line::from(vec![
        Span::styled(" ◆ SOL-ARB-BOT ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" │ ", dim), Span::styled("uptime ", dim), Span::styled(uptime, Style::default().fg(Color::White)),
        Span::styled("  │  gRPC ", dim), grpc_span,
        Span::styled("  │  net ", dim),
        Span::styled(format!("{:+.6} SOL", state.counters.net_profit_sol),
            Style::default().fg(net_color).add_modifier(Modifier::BOLD)),
        Span::styled("  │  confirmed ", dim),
        Span::styled(format!("{}", state.counters.confirmed), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled("/", dim),
        Span::styled(format!("{}", state.counters.submitted), Style::default().fg(Color::White)),
        Span::styled("  win ", dim),
        Span::styled(win, Style::default().fg(Color::Yellow)),
        Span::styled("  bndl ", dim),
        Span::styled(bundle_rate, Style::default().fg(Color::Cyan)),
        Span::raw(" "),
    ]);

    f.render_widget(
        Paragraph::new(line).block(
            Block::default().borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))),
        area,
    );
}

// ── Body ─────────────────────────────────────────────────────────────

fn draw_body(f: &mut Frame, state: &DashState, log_scroll: usize, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(10)])
        .split(area);
    draw_gauges(f, state, cols[0]);
    draw_log(f, state, log_scroll, cols[1]);
}

fn draw_gauges(f: &mut Frame, state: &DashState, area: Rect) {
    f.render_widget(
        Block::default().title(" Pipeline ").borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
        area,
    );

    let inner = Rect {
        x:      area.x + 1,
        y:      area.y + 1,
        width:  area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    let c = &state.counters;
    let rows: &[(&str, String, Color)] = &[
        ("Pools detected ", c.pools_detected.to_string(),  Color::Cyan),
        ("Evaluated      ", c.evaluated.to_string(),       Color::White),
        ("Profitable     ", c.profitable.to_string(),      Color::Yellow),
        ("Submitted      ", c.submitted.to_string(),       Color::White),
        ("Confirmed      ", c.confirmed.to_string(),       Color::Green),
        ("Failed         ", c.failed.to_string(),          Color::Red),
        ("Sim rejected   ", c.sim_rejected.to_string(),    Color::Magenta),
        ("Stale rejected ", c.stale_rejected.to_string(),  Color::Magenta),
        ("Scanner active ", state.scanner_active.to_string(), Color::Cyan),
        ("Bundles sent   ", c.bundles_sent.to_string(),    Color::Cyan),
        ("Bundles landed ", c.bundles_landed.to_string(),  Color::Green),
        ("Bndl timeout   ", c.bundles_timed_out.to_string(), Color::Yellow),
    ];

    for (i, (label, val, color)) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height { break; }
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
        let [left, right] = split_h(row, inner.width.saturating_sub(7), 7);
        f.render_widget(Paragraph::new(Span::styled(*label, Style::default().fg(Color::DarkGray))), left);
        f.render_widget(
            Paragraph::new(Span::styled(val.as_str(),
                Style::default().fg(*color).add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Right),
            right,
        );
    }

    let gauge_y = inner.y + rows.len() as u16 + 1;
    if gauge_y < inner.y + inner.height && c.submitted > 0 {
        let ratio = (c.confirmed as f64 / c.submitted as f64).min(1.0);
        f.render_widget(
            Gauge::default()
                .label(format!("Win {:.1}%", ratio * 100.0))
                .ratio(ratio)
                .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
                .style(Style::default().fg(Color::White)),
            Rect { x: inner.x, y: gauge_y, width: inner.width, height: 1 },
        );
    }

    let bundle_gauge_y = gauge_y + 2;
    if bundle_gauge_y < inner.y + inner.height && c.bundles_sent > 0 {
        let ratio = (c.bundles_landed as f64 / c.bundles_sent as f64).min(1.0);
        f.render_widget(
            Gauge::default()
                .label(format!("Bndl {:.1}%", ratio * 100.0))
                .ratio(ratio)
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
                .style(Style::default().fg(Color::White)),
            Rect { x: inner.x, y: bundle_gauge_y, width: inner.width, height: 1 },
        );
    }
}

fn draw_log(f: &mut Frame, state: &DashState, log_scroll: usize, area: Rect) {
    let log_len       = state.log.len();
    let height        = area.height.saturating_sub(2) as usize;
    let visible_start = if log_scroll >= log_len {
        0
    } else {
        log_len.saturating_sub(height + log_scroll)
    };

    let lines: Vec<Line> = state.log.iter()
        .skip(visible_start).take(height)
        .map(|e| {
            let (badge, bc) = match e.level {
                LogLevel::Info    => ("INFO ", Color::Cyan),
                LogLevel::Warn    => ("WARN ", Color::Yellow),
                LogLevel::Error   => ("ERR  ", Color::Red),
                LogLevel::Success => ("OK   ", Color::Green),
            };
            let age = e.ts.elapsed().as_secs();
            let ts  = if age < 60 { format!("{:>3}s", age) } else { format!("{:>2}m", age / 60) };
            Line::from(vec![
                Span::styled(ts,   Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(badge, Style::default().fg(bc).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<8} ", e.subsystem), Style::default().fg(Color::DarkGray)),
                Span::styled(e.msg.clone(), Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let title = if log_scroll > 0 {
        format!(" Activity Log [↑{} lines above live] ", log_scroll)
    } else {
        " Activity Log [live ▼] ".to_string()
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)))
            .wrap(Wrap { trim: true }),
        area,
    );
}

// ── Confirmed opportunities table ────────────────────────────────────

fn draw_opp_table(f: &mut Frame, state: &DashState, table_state: &mut TableState, area: Rect) {
    let header = Row::new([
        Cell::from("Token")          .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Cell::from("Net Profit (SOL)").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Cell::from("Signature")       .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Cell::from("Age")             .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]).height(1);

    let max  = area.height.saturating_sub(3) as usize;
    let rows: Vec<Row> = state.opportunities.iter().take(max).map(|o| {
        let pc = if o.profit_sol >= 0.0 { Color::Green } else { Color::Red };
        Row::new([
            Cell::from(o.token.clone())                         .style(Style::default().fg(Color::White)),
            Cell::from(format!("{:+.6}", o.profit_sol))         .style(Style::default().fg(pc).add_modifier(Modifier::BOLD)),
            Cell::from(o.signature.clone())                     .style(Style::default().fg(Color::DarkGray)),
            Cell::from(format!("{}s", o.ts.elapsed().as_secs())).style(Style::default().fg(Color::DarkGray)),
        ])
    }).collect();

    f.render_stateful_widget(
        Table::new(rows, [
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Min(24),
            Constraint::Length(6),
        ])
        .header(header)
        .block(Block::default().title(" Confirmed Opportunities ").borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        table_state,
    );
}

// ── Footer ───────────────────────────────────────────────────────────

fn draw_footer(f: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [q] quit  ",         dim),
            Span::styled("[↑/↓] scroll  ",      dim),
            Span::styled("[PgUp/PgDn] page  ",  dim),
            Span::styled("[End] live tail",      dim),
        ])),
        area,
    );
}

// ── Utilities ────────────────────────────────────────────────────────

/// Truncate `s` to at most `max` *characters*, appending "…" if truncated.
///
/// `s[..n]` with a byte index that falls inside a multi-byte character causes
/// a panic.  We resolve the cut point via `char_indices` so the boundary is
/// always valid, regardless of whether the input is ASCII or arbitrary UTF-8.
fn shorten(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }
    // Find the byte offset of the (max-1)-th character to leave room for "…".
    let cut = s
        .char_indices()
        .nth(max.saturating_sub(1))
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len());
    format!("{}…", &s[..cut])
}

fn split_h(area: Rect, left_w: u16, right_w: u16) -> [Rect; 2] {
    let left  = Rect { x: area.x,          y: area.y, width: left_w,  height: area.height };
    let right = Rect { x: area.x + left_w, y: area.y, width: right_w, height: area.height };
    [left, right]
}
