/// Overlay/popup draw functions — palette, slash-complete, file picker, session browser.
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use super::{AppState, FilePickerState, SessionBrowserState, slash_filtered,
            HookWizardState, WizardStep, PlanSymbolPickerState};

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
    let matches = slash_filtered(&state.input); // state.input used in SlashComplete mode
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

// ── Plan symbol picker ────────────────────────────────────────────────────────

/// Overlay for picking graph symbols (fn, struct, enum, trait) to pre-load into the planner.
/// Shown between `/plan task` input and actual plan generation. User searches by name,
/// marks symbols with Space/Enter, then presses Enter on an empty query to start planning.
pub fn draw_plan_symbol_picker(f: &mut Frame, picker: &PlanSymbolPickerState, area: Rect) {
    let width = 70u16.min(area.width.saturating_sub(4));
    let height = 22u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let n_picked = picker.picked.len();
    let title = if n_picked == 0 {
        " ◆ plan — attach symbols  (optional) ".to_string()
    } else {
        format!(" ◆ plan — {} symbol{} attached ", n_picked, if n_picked == 1 { "" } else { "s" })
    };

    let title_color = if n_picked > 0 { Color::Rgb(180, 140, 255) } else { Color::Rgb(140, 120, 200) };

    let outer = Block::default()
        .title(Span::styled(title, Style::default().fg(title_color).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Rgb(80, 60, 160)))
        .style(Style::default().bg(Color::Rgb(10, 8, 20)));

    let inner = outer.inner(popup_area);
    f.render_widget(outer, popup_area);

    // Layout: search bar | list | footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // search bar
            Constraint::Min(0),    // symbol list
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    // Search bar
    let search_line = Line::from(vec![
        Span::styled("  ❯ ", Style::default().fg(Color::Rgb(120, 100, 220)).add_modifier(Modifier::BOLD)),
        Span::styled(picker.query.clone(), Style::default().fg(Color::White)),
        Span::styled("_", Style::default().fg(Color::Rgb(120, 100, 220))),
    ]);
    f.render_widget(Paragraph::new(search_line), chunks[0]);

    // Symbol list
    let filtered = picker.filtered();
    let total = filtered.len();
    let list_area = chunks[1];
    let visible_height = list_area.height as usize;

    // Scroll to keep selected visible
    let sel = picker.selected.min(total.saturating_sub(1));
    let skip = if sel >= visible_height { sel - visible_height + 1 } else { 0 };

    let items: Vec<ListItem<'static>> = filtered
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible_height)
        .map(|(i, sym)| {
            let is_sel = i == sel;
            let is_picked = picker.is_picked(sym);

            let check = if is_picked { "✓ " } else { "  " };
            let check_color = if is_picked { Color::Rgb(120, 220, 120) } else { Color::DarkGray };

            let kind_color = match sym.kind.as_str() {
                "fn"     => Color::Rgb(100, 160, 255),
                "struct" => Color::Rgb(220, 160, 80),
                "enum"   => Color::Rgb(200, 100, 180),
                "trait"  => Color::Rgb(80, 200, 180),
                _        => Color::Rgb(160, 160, 160),
            };

            let (bg, name_fg, loc_fg) = if is_sel {
                (Color::Rgb(30, 22, 55), Color::White, Color::Rgb(160, 150, 210))
            } else {
                (Color::Reset, Color::Rgb(210, 205, 240), Color::Rgb(90, 80, 130))
            };

            // Trim file path to last two segments for readability
            let file_short = {
                let p = std::path::Path::new(&sym.file);
                let parts: Vec<_> = p.components().collect();
                if parts.len() >= 2 {
                    format!("{}/{}", parts[parts.len()-2].as_os_str().to_string_lossy(),
                                     parts[parts.len()-1].as_os_str().to_string_lossy())
                } else {
                    sym.file.clone()
                }
            };

            ListItem::new(Line::from(vec![
                Span::styled(check, Style::default().fg(check_color).bg(bg)),
                Span::styled(format!("{:<7}", sym.kind), Style::default().fg(kind_color).bg(bg)),
                Span::styled(format!("{:<30}", sym.name), Style::default().fg(name_fg).bg(bg).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}:{}", file_short, sym.start_line), Style::default().fg(loc_fg).bg(bg)),
            ]))
        })
        .collect();

    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  no symbols match",
                Style::default().fg(Color::DarkGray),
            )),
            list_area,
        );
    } else {
        // Scroll position indicator
        let scroll_info = if total > visible_height {
            format!("  {}/{total}", sel + 1)
        } else {
            String::new()
        };
        if !scroll_info.is_empty() {
            let info_area = Rect { x: list_area.x + list_area.width.saturating_sub(12), y: list_area.y, width: 12, height: 1 };
            f.render_widget(
                Paragraph::new(Span::styled(scroll_info, Style::default().fg(Color::Rgb(60, 55, 100)))),
                info_area,
            );
        }
        f.render_widget(List::new(items), list_area);
    }

    // Footer hint
    let hint = if n_picked > 0 {
        Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(Color::Rgb(100, 90, 160))),
            Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Space/Enter", Style::default().fg(Color::Rgb(180, 140, 255))),
            Span::styled(" toggle  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Rgb(120, 220, 120))),
            Span::styled(" (empty query) start  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Rgb(160, 140, 200))),
            Span::styled(" skip picker", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(Color::Rgb(100, 90, 160))),
            Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Space/Enter", Style::default().fg(Color::Rgb(180, 140, 255))),
            Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Rgb(120, 220, 120))),
            Span::styled(" (empty) start without symbols  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Rgb(160, 140, 200))),
            Span::styled(" skip", Style::default().fg(Color::DarkGray)),
        ])
    };
    f.render_widget(Paragraph::new(hint), chunks[2]);
}

// ── Hook setup wizard ─────────────────────────────────────────────────────────

pub fn draw_hook_wizard(f: &mut Frame, wiz: &HookWizardState, area: Rect) {
    let width = 66u16.min(area.width.saturating_sub(4));
    let height = 20u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect { x, y, width, height };

    f.render_widget(Clear, popup_area);

    let step_label = match wiz.step {
        WizardStep::EnterName           => " 1/7 name ",
        WizardStep::EnterOnEdit         => " 2/7 on_edit ",
        WizardStep::EnterOnTaskDone     => " 3/7 on_task_done ",
        WizardStep::EnterOnPlanStepDone => " 4/7 on_plan_step_done ",
        WizardStep::EnterOnSessionStart => " 5/7 on_session_start ",
        WizardStep::EnterOnSessionEnd   => " 6/7 on_session_end ",
        WizardStep::Confirm             => " 7/7 confirm ",
    };

    let outer = Block::default()
        .title(Span::styled(
            format!(" ⚙  New Hook Config  ·  {step_label}"),
            Style::default().fg(Color::Rgb(80, 200, 120)).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Rgb(60, 160, 90)))
        .style(Style::default().bg(Color::Rgb(8, 12, 10)));

    let inner = outer.inner(popup_area);
    f.render_widget(outer, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // description
            Constraint::Length(3), // input field
            Constraint::Min(0),    // preview / info
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    match wiz.step {
        WizardStep::EnterName => {
            let desc = vec![
                Line::from(Span::styled("  Give this hook config a name.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  It will be saved as [hooks.<name>] in config.toml.", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" name ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.name_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );

            f.render_widget(
                Paragraph::new(Span::styled(
                    "  e.g.  rust  typescript  myproject",
                    Style::default().fg(Color::Rgb(70, 100, 80)),
                )),
                chunks[2],
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → cancel", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::EnterOnEdit => {
            let desc = vec![
                Line::from(Span::styled("  on_edit — run after every file edit.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  Separate multiple commands with commas.", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" on_edit (optional) ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.on_edit_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );

            f.render_widget(
                Paragraph::new(Span::styled(
                    "  e.g.  cargo check -q",
                    Style::default().fg(Color::Rgb(70, 100, 80)),
                )),
                chunks[2],
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → back", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::EnterOnTaskDone => {
            let desc = vec![
                Line::from(Span::styled("  on_task_done — run after each completed task.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  Output shown in TUI (not injected into context).", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" on_task_done (optional) ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.on_task_done_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );

            f.render_widget(
                Paragraph::new(Span::styled(
                    "  e.g.  cargo test -q 2>&1 | tail -5",
                    Style::default().fg(Color::Rgb(70, 100, 80)),
                )),
                chunks[2],
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → back", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::EnterOnPlanStepDone => {
            let desc = vec![
                Line::from(Span::styled("  on_plan_step_done — run after each plan step.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  Useful for incremental checks during /plan runs.", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" on_plan_step_done (optional) ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.on_plan_step_done_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → back", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::EnterOnSessionStart => {
            let desc = vec![
                Line::from(Span::styled("  on_session_start — run when the TUI launches.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  e.g. print a welcome banner or activate a venv.", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" on_session_start (optional) ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.on_session_start_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → back", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::EnterOnSessionEnd => {
            let desc = vec![
                Line::from(Span::styled("  on_session_end — run when the TUI exits.", Style::default().fg(Color::Rgb(180, 180, 200)))),
                Line::from(Span::styled("  e.g. deactivate a venv or write a summary.", Style::default().fg(Color::Rgb(100, 100, 120)))),
                Line::from(Span::raw("")),
            ];
            f.render_widget(Paragraph::new(desc), chunks[0]);

            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(Color::Rgb(80, 180, 110)))
                .title(Span::styled(" on_session_end (optional) ", Style::default().fg(Color::Rgb(80, 200, 120))));
            let input_inner = input_block.inner(chunks[1]);
            f.render_widget(input_block, chunks[1]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}_", wiz.on_session_end_input),
                    Style::default().fg(Color::White),
                )),
                input_inner,
            );
            f.render_widget(
                Paragraph::new(Span::styled("  Enter → next   Esc → back", Style::default().fg(Color::Rgb(60, 60, 80)))),
                chunks[3],
            );
        }
        WizardStep::Confirm => {
            f.render_widget(
                Paragraph::new(Span::styled("  Review and save:", Style::default().fg(Color::Rgb(180, 180, 200)))),
                chunks[0],
            );

            let field_color = Color::Rgb(100, 180, 240);
            let mut preview = vec![
                Line::from(Span::styled(
                    format!("  [hooks.{}]", wiz.name_input),
                    Style::default().fg(Color::Rgb(80, 200, 120)).add_modifier(Modifier::BOLD),
                )),
            ];
            let fields = [
                ("on_edit           ", &wiz.on_edit_input),
                ("on_task_done      ", &wiz.on_task_done_input),
                ("on_plan_step_done ", &wiz.on_plan_step_done_input),
                ("on_session_start  ", &wiz.on_session_start_input),
                ("on_session_end    ", &wiz.on_session_end_input),
            ];
            let mut any = false;
            for (label, val) in &fields {
                if !val.trim().is_empty() {
                    preview.push(Line::from(Span::styled(
                        format!("  {label}= \"{}\"", val.trim()),
                        Style::default().fg(field_color),
                    )));
                    any = true;
                }
            }
            if !any {
                preview.push(Line::from(Span::styled(
                    "  (no commands — empty hook config)",
                    Style::default().fg(Color::Rgb(140, 100, 60)),
                )));
            }
            f.render_widget(Paragraph::new(preview), chunks[2]);

            f.render_widget(
                Paragraph::new(Span::styled(
                    "  y / Enter → save   n → cancel   Esc → back",
                    Style::default().fg(Color::Rgb(60, 60, 80)),
                )),
                chunks[3],
            );
        }
    }
}
