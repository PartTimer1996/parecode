/// Config tab — read-only view of the active profile, hooks, MCP servers, and conventions.
///
/// Shortcuts:
///   e — open config.toml in $EDITOR (full TOML editing for hooks, MCP, new profiles, etc.)
///   p — switch profile (interactive picker)
///   h — toggle hooks on/off for this session
///   j/↓ k/↑ — scroll
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph},
};

use super::AppState;

pub fn draw(f: &mut Frame, state: &AppState, area: Rect) {
    // Split area into scrollable content + fixed footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);

    let content_area = chunks[0];
    let footer_area = chunks[1];

    // ── Build content items ───────────────────────────────────────────────────
    let mut items: Vec<ListItem<'static>> = Vec::new();

    let heading = |s: &str| -> ListItem<'static> {
        ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                s.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
    };
    let dim = |s: String| -> ListItem<'static> {
        ListItem::new(Line::from(vec![
            Span::raw("    "),
            Span::styled(s, Style::default().fg(Color::Rgb(65, 60, 95))),
        ]))
    };
    let blank = || ListItem::new(Line::raw(""));

    // ── Profile header ────────────────────────────────────────────────────────
    items.push(blank());
    items.push(heading(&format!("Profile: {}  (active)", state.profile)));
    items.push(blank());

    // ── Settings ──────────────────────────────────────────────────────────────
    items.push(heading("Settings"));
    items.push(kv("endpoint", &state.endpoint));
    items.push(kv("model", &state.model));
    items.push(kv("context_tokens", &state.context_tokens.to_string()));
    items.push(kv(
        "api_key",
        if state.endpoint.contains("localhost") || state.endpoint.contains("127.0.0.1") {
            "(not required)"
        } else {
            "(set via config/env)"
        },
    ));
    items.push(kv(
        "auto_commit",
        if state.auto_commit { "true" } else { "false" },
    ));
    items.push(kv(
        "git_context",
        if state.git_context_enabled {
            "true"
        } else {
            "false"
        },
    ));
    items.push(blank());

    // ── Hooks ─────────────────────────────────────────────────────────────────
    items.push(heading("Hooks"));

    let hook_status = if state.hooks_disabled_profile {
        "disabled (hooks_disabled = true in profile)"
    } else if !state.hooks_enabled {
        "off  (press h to re-enable)"
    } else {
        "on  (press h to disable)"
    };
    items.push(kv("status", hook_status));

    let hc = &state.hooks_config;
    items.push(hook_line("on_edit", &hc.on_edit));
    items.push(hook_line("on_task_done", &hc.on_task_done));
    items.push(hook_line("on_plan_step_done", &hc.on_plan_step_done));
    items.push(hook_line("on_session_start", &hc.on_session_start));
    items.push(hook_line("on_session_end", &hc.on_session_end));
    items.push(blank());

    // ── MCP Servers ───────────────────────────────────────────────────────────
    items.push(heading("MCP Servers"));
    if state.mcp_server_names.is_empty() {
        items.push(dim("(none configured)".to_string()));
    } else {
        for name in &state.mcp_server_names {
            items.push(kv(name, "configured"));
        }
    }
    items.push(blank());

    // ── Conventions ───────────────────────────────────────────────────────────
    items.push(heading("Conventions"));
    let cwd = super::cwd_str();
    let conv_path = std::path::Path::new(&cwd).join(".parecode/conventions.md");
    if conv_path.exists() {
        items.push(kv("file", &conv_path.display().to_string()));
        // Show first few lines as preview
        if let Ok(content) = std::fs::read_to_string(&conv_path) {
            for line in content.lines().take(8) {
                items.push(dim(format!("  {line}")));
            }
        }
    } else {
        items.push(dim(format!(
            "{} — not found",
            conv_path.display()
        )));
        items.push(dim("  Use /init to generate one.".to_string()));
    }
    items.push(blank());

    // ── Clamp scroll ──────────────────────────────────────────────────────────
    let total = items.len();
    let visible = content_area.height as usize;
    let max_scroll = total.saturating_sub(visible);
    let scroll = state.config_scroll.min(max_scroll);

    // Apply scroll offset
    let visible_items: Vec<ListItem<'static>> = items
        .into_iter()
        .skip(scroll)
        .take(visible)
        .collect();

    let list = List::new(visible_items).block(
        Block::default().style(Style::default().bg(Color::Rgb(8, 8, 14))),
    );
    f.render_widget(list, content_area);

    // ── Scroll indicator ──────────────────────────────────────────────────────
    if max_scroll > 0 && scroll < max_scroll {
        let indicator = Paragraph::new(Line::from(vec![
            Span::styled(
                "    ↓ more (j/↓ to scroll) ",
                Style::default().fg(Color::Rgb(60, 55, 90)),
            ),
        ]))
        .style(Style::default().bg(Color::Rgb(8, 8, 14)));
        // Render at last row of content_area
        let ind_area = Rect {
            y: content_area.y + content_area.height.saturating_sub(1),
            height: 1,
            ..content_area
        };
        f.render_widget(indicator, ind_area);
    }

    // ── Footer — key hints (always visible) ───────────────────────────────────
    let footer_line = Line::from(vec![
        Span::raw("  "),
        Span::styled("e", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" edit   ", Style::default().fg(Color::Rgb(100, 95, 140))),
        Span::styled("p", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" profile   ", Style::default().fg(Color::Rgb(100, 95, 140))),
        Span::styled("h", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" hooks   ", Style::default().fg(Color::Rgb(100, 95, 140))),
        Span::styled("j/k", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" scroll", Style::default().fg(Color::Rgb(100, 95, 140))),
    ]);

    let footer = Paragraph::new(footer_line)
        .style(Style::default().bg(Color::Rgb(12, 12, 20)));
    f.render_widget(footer, footer_area);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn kv(k: &str, v: &str) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("    {k:<18}"),
            Style::default().fg(Color::Rgb(100, 95, 140)),
        ),
        Span::styled(v.to_string(), Style::default().fg(Color::White)),
    ]))
}

fn hook_line(label: &str, cmds: &[String]) -> ListItem<'static> {
    if cmds.is_empty() {
        ListItem::new(Line::from(vec![
            Span::styled(
                format!("    {label:<18}"),
                Style::default().fg(Color::Rgb(100, 95, 140)),
            ),
            Span::styled("—", Style::default().fg(Color::Rgb(55, 50, 80))),
        ]))
    } else {
        ListItem::new(Line::from(vec![
            Span::styled(
                format!("    {label:<18}"),
                Style::default().fg(Color::Rgb(100, 95, 140)),
            ),
            Span::styled(
                cmds.join("  ·  "),
                Style::default().fg(Color::Rgb(180, 200, 140)),
            ),
        ]))
    }
}
