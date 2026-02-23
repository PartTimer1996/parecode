/// Git tab — shows checkpoint info, diff stat, and action hints.
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::AppState;
use crate::git::GitRepo;

/// Refresh Git tab content from the repo. Call when switching to Tab::Git.
pub fn load_git_tab(state: &mut AppState) {
    if let Some(repo) = GitRepo::open(std::path::Path::new(".")) {
        state.git_checkpoints = repo.list_checkpoints().unwrap_or_default();
        let ref_pt = state.last_checkpoint_hash.as_deref().unwrap_or("HEAD");
        state.git_stat_content = repo.diff_stat_from(ref_pt).unwrap_or_default();
    }
}

pub fn draw(f: &mut Frame, state: &AppState, area: Rect) {
    // Split into: header (checkpoint info), stat area, action bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // checkpoint summary
            Constraint::Min(0),     // diff stat
            Constraint::Length(1),  // action hints
        ])
        .split(area);

    draw_checkpoint_header(f, state, chunks[0]);
    draw_diff_stat(f, state, chunks[1]);
    draw_action_bar(f, state, chunks[2]);
}

fn draw_checkpoint_header(f: &mut Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // Title
    lines.push(Line::from(vec![
        Span::styled(
            " ⎇  Git",
            Style::default()
                .fg(Color::Rgb(100, 180, 255))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::raw(""));

    // Last checkpoint
    if let Some(hash) = &state.last_checkpoint_hash {
        let short = &hash[..hash.len().min(8)];
        if let Some(cp) = state.git_checkpoints.first() {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Checkpoint: ",
                    Style::default().fg(Color::Rgb(120, 120, 150)),
                ),
                Span::styled(
                    cp.message.clone(),
                    Style::default().fg(Color::Rgb(200, 200, 230)),
                ),
                Span::styled(
                    format!("  [{}]", short),
                    Style::default().fg(Color::Rgb(80, 80, 110)),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Checkpoint: ",
                    Style::default().fg(Color::Rgb(120, 120, 150)),
                ),
                Span::styled(
                    short.to_string(),
                    Style::default().fg(Color::Rgb(200, 200, 230)),
                ),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  No checkpoint yet — run a task first",
            Style::default().fg(Color::Rgb(80, 80, 100)),
        )));
    }

    // Recent checkpoints list
    if !state.git_checkpoints.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  Recent checkpoints:",
            Style::default().fg(Color::Rgb(100, 100, 130)),
        )));
        for (i, cp) in state.git_checkpoints.iter().take(2).enumerate() {
            let age = format_age(cp.timestamp);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    [{}] ", i + 1),
                    Style::default().fg(Color::Rgb(80, 80, 110)),
                ),
                Span::styled(
                    cp.short_hash.clone(),
                    Style::default().fg(Color::Rgb(100, 140, 200)),
                ),
                Span::styled(
                    format!("  {}  ", age),
                    Style::default().fg(Color::Rgb(80, 80, 100)),
                ),
                Span::styled(
                    cp.message.chars().take(48).collect::<String>(),
                    Style::default().fg(Color::Rgb(150, 150, 180)),
                ),
            ]));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Rgb(6, 6, 12)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_diff_stat(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(30, 30, 50)))
        .style(Style::default().bg(Color::Rgb(6, 6, 12)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.git_stat_content.trim().is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  no changes",
                Style::default().fg(Color::Rgb(60, 60, 80)),
            )))
            .style(Style::default().bg(Color::Rgb(6, 6, 12))),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = state
        .git_stat_content
        .lines()
        .map(|line| {
            let fg = if line.contains("insertion") || (line.contains('|') && line.contains('+')) {
                Color::Rgb(80, 180, 80)
            } else if line.contains("deletion") || (line.contains('|') && line.contains('-')) {
                Color::Rgb(200, 80, 80)
            } else if line.contains("changed") || line.contains("file") {
                // Summary line
                Color::Rgb(180, 180, 100)
            } else {
                Color::Rgb(120, 120, 150)
            };
            Line::from(vec![
                Span::raw("  "),
                Span::styled(line.to_string(), Style::default().fg(fg)),
            ])
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Rgb(6, 6, 12)))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_action_bar(f: &mut Frame, _state: &AppState, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "  [d] full diff  ",
            Style::default().fg(Color::Rgb(80, 140, 200)),
        ),
        Span::styled(
            "[u] undo  ",
            Style::default().fg(Color::Rgb(200, 120, 80)),
        ),
        Span::styled(
            "[1] back to chat  ",
            Style::default().fg(Color::Rgb(80, 80, 100)),
        ),
        Span::styled(
            "/undo [n] to revert to Nth checkpoint",
            Style::default().fg(Color::Rgb(60, 60, 80)),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(6, 6, 12))),
        area,
    );
}

/// Format a Unix timestamp as a human-readable age string ("2 min ago", "3h ago", etc.)
fn format_age(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - timestamp).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
