use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph, List, ListItem},
    Terminal,
};
use std::{io, sync::{Arc, Mutex}, time::{Duration, Instant}};
use tokio::sync::mpsc;

pub struct TuiState {
    pub logs: Vec<String>,
    pub opportunities_found: u64,
    pub bundles_submitted: u64,
    pub total_profit_sol: f64,
    pub _last_update: Instant,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            logs: Vec::new(),
            opportunities_found: 0,
            bundles_submitted: 0,
            total_profit_sol: 0.0,
            _last_update: Instant::now(),
        }
    }
}

pub async fn run_tui(state: Arc<Mutex<TuiState>>, mut rx: mpsc::Receiver<String>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(10),
                ])
                .split(f.area());

            let s = state.lock().unwrap();
            
            // Stats Header
            let stats = format!(
                "Opportunities: {} | Bundles: {} | Total Profit: {:.4} SOL",
                s.opportunities_found, s.bundles_submitted, s.total_profit_sol
            );
            let header = Paragraph::new(stats)
                .block(Block::default().borders(Borders::ALL).title("Stats"));
            f.render_widget(header, chunks[0]);

            // Logs
            let log_items: Vec<ListItem> = s.logs.iter().rev().take(chunks[1].height as usize).map(|l| {
                ListItem::new(l.as_str())
            }).collect();
            let logs_list = List::new(log_items)
                .block(Block::default().borders(Borders::ALL).title("Logs"));
            f.render_widget(logs_list, chunks[1]);

            // Help / Info
            let help = Paragraph::new("Press 'q' to quit | Monitoring Solana Arbitrage Bot")
                .block(Block::default().borders(Borders::ALL).title("Info"));
            f.render_widget(help, chunks[2]);
        })?;

        while let Ok(log) = rx.try_recv() {
            let mut s = state.lock().unwrap();
            s.logs.push(log);
            if s.logs.len() > 100 {
                s.logs.remove(0);
            }
        }

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if let KeyCode::Char('q') = key.code {
                    break;
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
