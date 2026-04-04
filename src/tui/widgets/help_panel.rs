use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Render the help / keybindings panel.
pub fn render_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard Shortcuts",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )),
        Line::from(""),
        key_line("q / Ctrl+C", "Quit the TUI and shut down the bot"),
        key_line("Tab / Shift+Tab", "Next / previous tab"),
        key_line("1 / 2 / 3 / 4", "Jump to Dashboard / Opportunities / Logs / Help"),
        key_line("p", "Pause / resume opportunity scanning display"),
        key_line("r", "Force a full redraw"),
        key_line("f", "Toggle log filter (cycles through presets)"),
        key_line("c", "Clear error banner"),
        key_line("↑ / ↓  or  k / j", "Scroll logs or opportunity table"),
        key_line("g / G", "Jump to top / bottom of current list"),
        key_line("m", "Toggle mouse support"),
        Line::from(""),
        Line::from(Span::styled(
            "Colour Legend",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )),
        Line::from(""),
        color_line(Color::Green, "Green", "Healthy / profitable (≥ 0.01 SOL)"),
        color_line(Color::Yellow, "Yellow", "Warning / moderate profit (≥ 0.005 SOL)"),
        color_line(Color::Red, "Red", "Error / unhealthy"),
        color_line(Color::Cyan, "Cyan", "Info / selected"),
        color_line(Color::DarkGray, "Gray", "Debug / secondary info"),
        Line::from(""),
        Line::from(Span::styled(
            "Environment Variables",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )),
        Line::from(""),
        env_line("TUI_FPS", "Target render frames per second (default: 10)"),
        env_line("TUI_MOUSE", "Enable mouse support: true/false (default: false)"),
        env_line("TUI_COMPACT", "Compact layout for small terminals: true/false"),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(paragraph, area);
}

fn key_line(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<22}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

fn color_line(color: Color, name: &'static str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  ■ {name:<10}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

fn env_line(var: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {var:<20}"),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(desc.to_string()),
    ])
}
