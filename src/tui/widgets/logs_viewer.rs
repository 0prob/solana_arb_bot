use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};
use crate::tui::app::{App, LogLevel};

/// Render the colour-coded log viewer with optional filter.
pub fn render_logs(f: &mut Frame, area: Rect, app: &App) {
    let filtered = app.filtered_logs();
    let visible_height = area.height.saturating_sub(2) as usize;
    let total = filtered.len();

    // Auto-scroll to bottom unless the user has scrolled up.
    let scroll = if app.log_scroll == 0 {
        total.saturating_sub(visible_height)
    } else {
        app.log_scroll.min(total.saturating_sub(visible_height))
    };

    let items: Vec<ListItem> = filtered
        .iter()
        .skip(scroll)
        .take(visible_height)
        .map(|&idx| {
            let entry = &app.logs[idx];
            let level_color = match entry.level {
                LogLevel::Error => Color::Red,
                LogLevel::Warn => Color::Yellow,
                LogLevel::Info => Color::Green,
                LogLevel::Debug => Color::Blue,
                LogLevel::Trace => Color::DarkGray,
            };
            let level_str = match entry.level {
                LogLevel::Error => "ERROR",
                LogLevel::Warn => "WARN ",
                LogLevel::Info => "INFO ",
                LogLevel::Debug => "DEBUG",
                LogLevel::Trace => "TRACE",
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("[{level_str}] "),
                    Style::default()
                        .fg(level_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}: ", entry.target),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(entry.message.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let filter_hint = match &app.log_filter {
        Some(f) => format!(" [filter: {f}]"),
        None => String::new(),
    };
    let title = format!("Logs ({total} lines){filter_hint}");

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, area);
}
