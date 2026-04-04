use ratatui::{
    layout::Rect,
    style::{Color, Style},
    symbols,
    widgets::{Block, Borders, Sparkline},
    Frame,
};
use crate::tui::app::App;

/// Render a sparkline chart of recent profit values (in micro-SOL).
pub fn render_sparkline(f: &mut Frame, area: Rect, app: &App) {
    let data: Vec<u64> = app.profit_sparkline.iter().copied().collect();
    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Profit History (micro-SOL)"),
        )
        .data(&data)
        .style(Style::default().fg(Color::Green))
        .bar_set(symbols::bar::NINE_LEVELS);
    f.render_widget(sparkline, area);
}
