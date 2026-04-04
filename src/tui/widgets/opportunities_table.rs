use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table},
    Frame,
};
use crate::tui::app::App;

/// Render the live opportunity table.
pub fn render_opportunities(f: &mut Frame, area: Rect, app: &App) {
    let header_cells = ["#", "Token", "Loan (SOL)", "Profit (SOL)", "Age (s)"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let total = app.opportunities.len();
    let rows: Vec<Row> = app
        .opportunities
        .iter()
        .rev()
        .enumerate()
        .map(|(i, opp)| {
            let age = opp.timestamp.elapsed().as_secs();
            let profit_color = if opp.profit_sol >= 0.01 {
                Color::Green
            } else if opp.profit_sol >= 0.005 {
                Color::Yellow
            } else {
                Color::White
            };
            Row::new(vec![
                Cell::from(format!("{}", total - i)),
                Cell::from(truncate_token(&opp.token)),
                Cell::from(format!("{:.4}", opp.loan_sol)),
                Cell::from(format!("{:.6}", opp.profit_sol))
                    .style(Style::default().fg(profit_color)),
                Cell::from(format!("{age}s")),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            ratatui::layout::Constraint::Length(5),
            ratatui::layout::Constraint::Length(10),
            ratatui::layout::Constraint::Length(12),
            ratatui::layout::Constraint::Length(14),
            ratatui::layout::Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Opportunities ({total})")),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_widget(table, area);
}

/// Shorten a base58 token address for display.
fn truncate_token(addr: &str) -> String {
    if addr.len() > 8 {
        format!("{}…{}", &addr[..4], &addr[addr.len() - 4..])
    } else {
        addr.to_string()
    }
}
