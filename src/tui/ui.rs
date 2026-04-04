#![cfg(feature = "tui")]
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};
use crate::tui::app::{ActiveTab, App};
use crate::tui::widgets::{
    header::render_header,
    help_panel::render_help,
    logs_viewer::render_logs,
    opportunities_table::render_opportunities,
    sparkline_chart::render_sparkline,
};

/// Top-level render entry point called inside `terminal.draw(|f| render(f, app))`.
pub fn render(f: &mut Frame, app: &App) {
    let size = f.area();

    if app.compact || size.height < 20 {
        render_compact(f, size, app);
        return;
    }

    // ── Outer layout: header | body | footer ─────────────────────────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header / tabs
            Constraint::Min(0),    // body
            Constraint::Length(3), // footer / status bar
        ])
        .split(size);

    render_header(f, outer[0], app);

    // ── Error banner overlays the body top if set ─────────────────────────
    let body_area = outer[1];
    if let Some((msg, _)) = &app.error_banner {
        let banner_area = Rect {
            x: body_area.x,
            y: body_area.y,
            width: body_area.width,
            height: 3,
        };
        let rest_area = Rect {
            x: body_area.x,
            y: body_area.y + 3,
            width: body_area.width,
            height: body_area.height.saturating_sub(3),
        };
        render_error_banner(f, banner_area, msg);
        render_tab_body(f, rest_area, app);
    } else {
        render_tab_body(f, body_area, app);
    }

    render_footer(f, outer[2], app);
}

fn render_tab_body(f: &mut Frame, area: Rect, app: &App) {
    match app.active_tab {
        ActiveTab::Dashboard => render_dashboard(f, area, app),
        ActiveTab::Opportunities => render_opportunities(f, area, app),
        ActiveTab::Logs => render_logs(f, area, app),
        ActiveTab::Help => render_help(f, area),
    }
}

/// Dashboard tab: stats + sparkline + recent bundles.
fn render_dashboard(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),  // stats grid
            Constraint::Length(8),  // sparkline
            Constraint::Min(0),     // recent bundles
        ])
        .split(area);

    render_stats_grid(f, chunks[0], app);
    render_sparkline(f, chunks[1], app);
    render_recent_bundles(f, chunks[2], app);
}

fn render_stats_grid(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let stat = |label: &str, value: String, color: Color| -> Paragraph<'static> {
        Paragraph::new(vec![
            Line::from(Span::styled(
                value,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(label.to_string()),
        )
    };

    f.render_widget(
        stat(
            "Opportunities",
            app.opportunities_found.to_string(),
            Color::Cyan,
        ),
        cols[0],
    );
    f.render_widget(
        stat(
            "Bundles Sent",
            app.bundles_submitted.to_string(),
            Color::Yellow,
        ),
        cols[1],
    );
    f.render_widget(
        stat(
            "Total Profit",
            format!("{:.6} SOL", app.total_profit_sol),
            Color::Green,
        ),
        cols[2],
    );
    f.render_widget(
        stat(
            "Render µs",
            app.render_time_us.to_string(),
            Color::DarkGray,
        ),
        cols[3],
    );
}

fn render_recent_bundles(f: &mut Frame, area: Rect, app: &App) {
    let header_cells = ["#", "Bundle ID", "Profit (SOL)", "Tip (SOL)", "Age (s)"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let total = app.bundles.len();
    let rows: Vec<Row> = app
        .bundles
        .iter()
        .rev()
        .enumerate()
        .map(|(i, b)| {
            let age = b.timestamp.elapsed().as_secs();
            let short_id = if b.bundle_id.len() > 12 {
                format!("{}…", &b.bundle_id[..12])
            } else {
                b.bundle_id.clone()
            };
            Row::new(vec![
                Cell::from(format!("{}", total - i)),
                Cell::from(short_id),
                Cell::from(format!("{:.6}", b.profit_sol))
                    .style(Style::default().fg(Color::Green)),
                Cell::from(format!("{:.6}", b.tip_sol))
                    .style(Style::default().fg(Color::Yellow)),
                Cell::from(format!("{age}s")),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(15),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Recent Bundles ({total})")),
    );

    f.render_widget(table, area);
}

fn render_error_banner(f: &mut Frame, area: Rect, msg: &str) {
    let p = Paragraph::new(Span::styled(
        format!(" ⚠  {msg}"),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    ))
    .block(Block::default().borders(Borders::ALL).title("Error"));
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let mouse_str = if app.mouse_enabled { "Mouse:ON" } else { "Mouse:OFF" };
    let pause_str = if app.paused { " | PAUSED" } else { "" };
    let filter_str = match &app.log_filter {
        Some(f) => format!(" | Filter:{f}"),
        None => String::new(),
    };
    let text = format!(
        " q:Quit  Tab:Nav  p:Pause  r:Refresh  f:Filter  c:ClearErr  m:Mouse  | {mouse_str}{pause_str}{filter_str}"
    );
    let footer = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, area);
}

/// Compact layout for terminals smaller than 20 rows.
fn render_compact(f: &mut Frame, area: Rect, app: &App) {
    let text = format!(
        "SOL-ARB | Opps:{} Bundles:{} Profit:{:.4} SOL | q:quit",
        app.opportunities_found, app.bundles_submitted, app.total_profit_sol
    );
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("SOL-ARB-BOT"));
    f.render_widget(p, area);
}
