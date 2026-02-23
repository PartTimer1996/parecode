/// Session sidebar — collapsible left panel showing recent sessions.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
};

use super::AppState;

pub fn draw_sidebar(f: &mut Frame, state: &AppState, area: Rect) {
    let focused = state.sidebar_focused;
    let border_color = if focused { Color::Cyan } else { Color::Rgb(40, 38, 60) };

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(6, 6, 12)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = inner.width as usize;
    let mut items: Vec<ListItem<'static>> = Vec::new();

    // Header
    let ctrl_hint = if focused { " Esc=exit" } else { " Tab=focus" };
    let header_pad = w.saturating_sub(9 + ctrl_hint.len());
    items.push(ListItem::new(Line::from(vec![
        Span::styled(" Sessions", Style::default().fg(Color::Rgb(100, 95, 150)).add_modifier(Modifier::BOLD)),
        Span::styled(" ".repeat(header_pad), Style::default()),
        Span::styled(ctrl_hint.to_string(), Style::default().fg(Color::Rgb(50, 47, 75))),
    ])));
    items.push(ListItem::new(Line::from(vec![
        Span::styled("─".repeat(w), Style::default().fg(Color::Rgb(35, 33, 55))),
    ])));

    if state.sidebar_entries.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(" no sessions", Style::default().fg(Color::Rgb(50, 47, 75))),
        ])));
    } else {
        for (i, entry) in state.sidebar_entries.iter().enumerate() {
            let selected = focused && i == state.sidebar_selected;

            // Colour scheme: current session = cyan; selected (focused) = bright highlight
            let (bg, bullet_fg, name_fg, meta_fg, preview_fg) = if entry.is_current && selected {
                (Color::Rgb(20, 40, 50), Color::Cyan, Color::Cyan, Color::Rgb(0, 200, 200), Color::Rgb(100, 200, 210))
            } else if entry.is_current {
                (Color::Rgb(10, 22, 30), Color::Cyan, Color::Cyan, Color::Rgb(0, 170, 170), Color::Rgb(80, 160, 170))
            } else if selected {
                (Color::Rgb(28, 26, 48), Color::Rgb(160, 155, 220), Color::White, Color::Rgb(140, 135, 200), Color::Rgb(160, 155, 200))
            } else {
                (Color::Reset, Color::Rgb(60, 57, 90), Color::Rgb(150, 145, 190), Color::Rgb(70, 67, 100), Color::Rgb(60, 58, 90))
            };

            let bullet = if entry.is_current { "●" } else { "○" };

            // Line 1: bullet + project name (left) + turn count (right)
            let count_str = format!("{}↩ ", entry.turn_count);
            let project_max = w.saturating_sub(3 + count_str.len()); // " ● " + count
            let project: String = entry.project.chars().take(project_max).collect();
            let gap = w.saturating_sub(3 + project.len() + count_str.len());
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {bullet} "), Style::default().fg(bullet_fg).bg(bg)),
                Span::styled(project, Style::default().fg(name_fg).bg(bg).add_modifier(if entry.is_current { Modifier::BOLD } else { Modifier::empty() })),
                Span::styled(" ".repeat(gap), Style::default().bg(bg)),
                Span::styled(count_str, Style::default().fg(meta_fg).bg(bg)),
            ])));

            // Line 2: timestamp (dimmer)
            let ts: String = entry.timestamp.chars().take(w.saturating_sub(2)).collect();
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("  {ts}"), Style::default().fg(meta_fg).bg(bg)),
            ])));

            // Line 3: preview
            let preview: String = entry.preview.chars().take(w.saturating_sub(3)).collect();
            if !preview.is_empty() {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!("  {preview}"), Style::default().fg(preview_fg).bg(bg)),
                ])));
            }

            // Divider
            items.push(ListItem::new(Line::from(vec![
                Span::styled("─".repeat(w), Style::default().fg(Color::Rgb(25, 23, 40))),
            ])));
        }
    }

    // Footer hint
    let new_label = " [+] New  /new";
    items.push(ListItem::new(Line::from(vec![
        Span::styled(new_label.to_string(), Style::default().fg(Color::Rgb(55, 52, 80))),
    ])));

    f.render_widget(List::new(items), inner);
}
