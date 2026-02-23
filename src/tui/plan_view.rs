/// Plan tab — full step list with status, reusing build_plan_card_items.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, Paragraph},
};

use super::AppState;

pub fn draw(f: &mut Frame, state: &AppState, area: Rect) {
    let bg = Block::default().style(Style::default().bg(Color::Rgb(8, 8, 14)));
    if state.plan_review.is_some() {
        let items = super::chat::build_plan_card_items(state, state.cost_per_mtok_input);
        f.render_widget(List::new(items).block(bg), area);
    } else {
        let lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("No active plan.", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    r#"Use /plan "task description" to generate one."#,
                    Style::default().fg(Color::Rgb(70, 65, 100)).add_modifier(Modifier::ITALIC),
                ),
            ]),
        ];
        f.render_widget(
            Paragraph::new(lines).block(bg),
            area,
        );
    }
}
