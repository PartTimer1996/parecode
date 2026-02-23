/// Overlay/popup draw functions — palette, slash-complete, file picker, session browser.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::{AppState, FilePickerState, SessionBrowserState, slash_filtered};

// ── Command palette ────────────────────────────────────────────────────────────

pub fn draw_palette(f: &mut Frame, state: &AppState, area: Rect) {
    use super::palette_commands;

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 14u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let commands = palette_commands();
    let query = state.palette_query.to_lowercase();

    let items: Vec<ListItem<'static>> = commands
        .iter()
        .filter(|c| {
            query.is_empty()
                || c.key.contains(query.as_str())
                || c.label.to_lowercase().contains(query.as_str())
        })
        .map(|c| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {:<14}", c.key),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(c.label.to_string(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let outer_block = Block::default()
        .title(Span::styled(
            " Command Palette ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer_block.inner(popup_area);
    f.render_widget(outer_block, popup_area);

    // Search bar at top of inner area
    let search_area = Rect { height: 1, ..inner };
    let list_area = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };

    let search_line = Line::from(vec![
        Span::styled("  ❯ ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(state.palette_query.clone()),
    ]);
    f.render_widget(Paragraph::new(search_line), search_area);

    let list = List::new(items);
    f.render_widget(list, list_area);
}

// ── Slash autocomplete ─────────────────────────────────────────────────────────

pub fn draw_slash_complete(f: &mut Frame, state: &AppState, area: Rect) {
    let matches = slash_filtered(&state.input);
    if matches.is_empty() {
        return;
    }

    let count = matches.len() as u16;
    let height = (count + 2).min(18).min(area.height.saturating_sub(4));
    let width = 62u16.min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    // Anchor above the input box (bottom-anchored like file picker)
    let y = area.y + area.height.saturating_sub(height + 4);
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let sel = state.slash_complete_selected;
    let items: Vec<ListItem<'static>> = matches
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let (key_style, label_style) = if i == sel {
                (
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )
            } else {
                (
                    Style::default().fg(Color::Cyan),
                    Style::default().fg(Color::DarkGray),
                )
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {:<16}", cmd.key), key_style),
                Span::styled(cmd.label.to_string(), label_style),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(Span::styled(
            " Commands  ↑↓ navigate  Tab/Enter complete  Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Scroll to keep selected visible
    let visible = inner.height as usize;
    let skip = if sel >= visible { sel - visible + 1 } else { 0 };

    let visible_items: Vec<ListItem<'static>> =
        items.into_iter().skip(skip).take(visible).collect();
    let list = List::new(visible_items);
    f.render_widget(list, inner);
}

// ── File picker ────────────────────────────────────────────────────────────────

pub fn draw_file_picker(f: &mut Frame, fp: &FilePickerState, area: Rect) {
    let width = 64u16.min(area.width.saturating_sub(4));
    let height = 18u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    // Anchor near the bottom (above the input box)
    let y = area.y + area.height.saturating_sub(height + 4);
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let filtered = fp.filtered();
    let total = filtered.len();

    let items: Vec<ListItem<'static>> = filtered
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let style = if i == fp.selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };
            // Split into dir + filename for visual clarity
            let p = std::path::Path::new(path.as_str());
            let dir = p.parent()
                .and_then(|d| {
                    let s = d.display().to_string();
                    if s.is_empty() { None } else { Some(format!("{s}/")) }
                })
                .unwrap_or_default();
            let fname = p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();

            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(dir, style.fg(Color::DarkGray)),
                Span::styled(fname, style),
            ]))
        })
        .collect();

    let title = if fp.query.is_empty() {
        format!(" @ files ({total}) ")
    } else {
        format!(" @ {} ({total}) ", fp.query)
    };

    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Footer hint
    let hint_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let list_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };

    // Scroll list to keep selected in view
    let visible = list_area.height as usize;
    let skip = if fp.selected >= visible {
        fp.selected - visible + 1
    } else {
        0
    };
    let sliced: Vec<ListItem<'static>> = items.into_iter().skip(skip).collect();
    let list = List::new(sliced);
    f.render_widget(list, list_area);

    let hint = Line::from(vec![
        Span::styled("  ↑↓ navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(hint), hint_area);
}

// ── Session browser ────────────────────────────────────────────────────────────

pub fn draw_session_browser(f: &mut Frame, browser: &SessionBrowserState, area: Rect) {
    let width = 72u16.min(area.width.saturating_sub(4));
    let height = 20u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " Session History ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(110, 90, 200)));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Footer hint
    let hint_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let list_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };

    if browser.entries.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  no sessions found", Style::default().fg(Color::DarkGray)),
            ])),
            list_area,
        );
    } else {
        let items: Vec<ListItem<'static>> = browser
            .entries
            .iter()
            .enumerate()
            .map(|(i, (id, _path, count, preview))| {
                let selected = i == browser.selected;
                // Parse timestamp prefix from id for a human-readable date
                let date_str = id
                    .splitn(2, '_')
                    .next()
                    .and_then(|ts| ts.parse::<i64>().ok())
                    .map(|ts| {
                        let dt = chrono::DateTime::from_timestamp(ts, 0)
                            .unwrap_or_default()
                            .with_timezone(&chrono::Local);
                        dt.format("%b %d %H:%M").to_string()
                    })
                    .unwrap_or_else(|| id.chars().take(16).collect());
                // Project name (part after first underscore)
                let project = id.splitn(2, '_').nth(1).unwrap_or(id.as_str());

                let (bg, date_fg, proj_fg, prev_fg) = if selected {
                    (
                        Color::Rgb(40, 35, 70),
                        Color::Rgb(160, 140, 255),
                        Color::White,
                        Color::Rgb(200, 195, 240),
                    )
                } else {
                    (
                        Color::Reset,
                        Color::Rgb(100, 90, 160),
                        Color::Rgb(180, 180, 220),
                        Color::DarkGray,
                    )
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  {date_str}  "),
                        Style::default().fg(date_fg).bg(bg),
                    ),
                    Span::styled(
                        format!("{project:<14}  "),
                        Style::default().fg(proj_fg).bg(bg).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{count}t  "),
                        Style::default().fg(Color::Rgb(110, 90, 200)).bg(bg),
                    ),
                    Span::styled(
                        preview.clone(),
                        Style::default().fg(prev_fg).bg(bg),
                    ),
                ]))
            })
            .collect();

        // Scroll to keep selected in view
        let visible = list_area.height as usize;
        let skip = if browser.selected >= visible {
            browser.selected - visible + 1
        } else {
            0
        };
        let sliced: Vec<ListItem<'static>> = items.into_iter().skip(skip).collect();
        f.render_widget(List::new(sliced), list_area);
    }

    let hint = Line::from(vec![
        Span::styled("  ↑↓ navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Rgb(160, 140, 255))),
        Span::styled(" load session  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Rgb(160, 140, 255))),
        Span::styled(" close", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(hint), hint_area);
}

// ── Full diff overlay ──────────────────────────────────────────────────────────

/// Full-screen overlay showing the complete `git diff` output with syntax colouring.
/// Opened by pressing `d` or `/diff`. Scrollable with j/k. Dismissed with d/Esc.
pub fn draw_diff_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    if !state.diff_overlay_visible || state.git_diff_content.is_empty() {
        return;
    }

    // 2-char inset from screen edges
    let overlay_area = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };

    f.render_widget(Clear, overlay_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " git diff  (d/Esc close · j/k scroll · PgDn/PgUp fast) ",
            Style::default()
                .fg(Color::Rgb(100, 180, 255))
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Rgb(50, 80, 140)))
        .style(Style::default().bg(Color::Rgb(8, 8, 14)));

    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);

    // Parse lines and apply diff colouring by line prefix
    let all_lines: Vec<Line> = state
        .git_diff_content
        .lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                // File header lines — purple bold
                Style::default()
                    .fg(Color::Rgb(180, 140, 255))
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with('+') {
                // Added lines — green
                Style::default().fg(Color::Rgb(80, 200, 80))
            } else if line.starts_with('-') {
                // Removed lines — red
                Style::default().fg(Color::Rgb(200, 80, 80))
            } else if line.starts_with("@@") {
                // Hunk header — cyan
                Style::default().fg(Color::Rgb(80, 160, 255))
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                // Diff meta — purple
                Style::default().fg(Color::Rgb(140, 100, 200))
            } else {
                // Context lines — dimmed
                Style::default().fg(Color::Rgb(140, 140, 160))
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect();

    let total_lines = all_lines.len();
    let visible_height = inner.height as usize;

    // Clamp scroll so we don't scroll past the end
    let scroll = state
        .diff_overlay_scroll
        .min(total_lines.saturating_sub(visible_height));

    let visible: Vec<Line> = all_lines.into_iter().skip(scroll).collect();

    f.render_widget(
        Paragraph::new(visible).style(Style::default().bg(Color::Rgb(8, 8, 14))),
        inner,
    );
}

// ── Profile picker ─────────────────────────────────────────────────────────────

pub fn draw_profile_picker(f: &mut Frame, state: &AppState, area: Rect) {
    let entries = &state.profile_picker_entries;
    if entries.is_empty() {
        return;
    }

    let count = entries.len() as u16;
    let height = (count + 2).min(16).min(area.height.saturating_sub(6));
    let width = 54u16.min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let sel = state.profile_picker_selected;
    let items: Vec<ListItem<'static>> = entries
        .iter()
        .enumerate()
        .map(|(i, (name, model))| {
            let active = *name == state.profile;
            let marker = if active { " ← " } else { "   " };
            let (name_style, model_style) = if i == sel {
                (
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )
            } else if active {
                (
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Rgb(100, 180, 220)),
                )
            } else {
                (
                    Style::default().fg(Color::White),
                    Style::default().fg(Color::DarkGray),
                )
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {name:<16}"), name_style),
                Span::styled(model.to_string(), model_style),
                Span::styled(marker.to_string(), name_style),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(Span::styled(
            " Switch Profile  ↑↓ navigate  Enter select  Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Scroll to keep selected visible
    let visible = inner.height as usize;
    let skip = if sel >= visible { sel - visible + 1 } else { 0 };

    let visible_items: Vec<ListItem<'static>> =
        items.into_iter().skip(skip).take(visible).collect();
    let list = List::new(visible_items);
    f.render_widget(list, inner);
}
