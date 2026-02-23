/// Config tab — active profile, endpoint, model, hooks, conventions.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph},
};

use super::AppState;

pub fn draw(f: &mut Frame, state: &AppState, area: Rect) {
    let mut items: Vec<ListItem<'static>> = Vec::new();

    let h = |s: &str| -> ListItem<'static> {
        ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(s.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]))
    };
    let kv = |k: &str, v: String| -> ListItem<'static> {
        ListItem::new(Line::from(vec![
            Span::styled(format!("    {k:<18}"), Style::default().fg(Color::Rgb(100, 95, 140))),
            Span::styled(v, Style::default().fg(Color::White)),
        ]))
    };
    let dim = |s: String| -> ListItem<'static> {
        ListItem::new(Line::from(vec![
            Span::raw("    "),
            Span::styled(s, Style::default().fg(Color::Rgb(65, 60, 95))),
        ]))
    };
    let blank = || ListItem::new(Line::raw(""));

    // ── Profile ───────────────────────────────────────────────────────────────
    items.push(blank());
    items.push(h("Profile"));
    items.push(kv("name", format!("{}  (active)", state.profile)));
    items.push(kv("endpoint", state.endpoint.clone()));
    items.push(kv("model", state.model.clone()));
    items.push(kv(
        "context",
        format!("{} tokens", state.context_tokens),
    ));
    items.push(blank());

    // ── Hooks ─────────────────────────────────────────────────────────────────
    items.push(h("Hooks"));

    let hook_status = if state.hooks_disabled_profile {
        "disabled (hooks_disabled = true in profile)"
    } else if !state.hooks_enabled {
        "off  (/hooks on to re-enable)"
    } else {
        "on"
    };
    items.push(kv("status", hook_status.to_string()));

    let hook_line = |label: &str, cmds: &[String]| -> ListItem<'static> {
        if cmds.is_empty() {
            ListItem::new(Line::from(vec![
                Span::styled(format!("    {label:<18}"), Style::default().fg(Color::Rgb(100, 95, 140))),
                Span::styled("—", Style::default().fg(Color::Rgb(55, 50, 80))),
            ]))
        } else {
            ListItem::new(Line::from(vec![
                Span::styled(format!("    {label:<18}"), Style::default().fg(Color::Rgb(100, 95, 140))),
                Span::styled(cmds.join("  ·  "), Style::default().fg(Color::Rgb(180, 200, 140))),
            ]))
        }
    };

    let hc = &state.hooks_config;
    items.push(hook_line("on_edit", &hc.on_edit));
    items.push(hook_line("on_task_done", &hc.on_task_done));
    items.push(hook_line("on_plan_step_done", &hc.on_plan_step_done));
    items.push(hook_line("on_session_start", &hc.on_session_start));
    items.push(hook_line("on_session_end", &hc.on_session_end));
    items.push(blank());

    // ── Conventions ───────────────────────────────────────────────────────────
    items.push(h("Conventions"));
    let cwd = super::cwd_str();
    let conv_path = std::path::Path::new(&cwd).join(".forge/conventions.md");
    if conv_path.exists() {
        items.push(kv("file", conv_path.display().to_string()));
        items.push(dim("  [loaded]".to_string()));
    } else {
        items.push(dim(format!("{} — not found", conv_path.display())));
        items.push(dim("  Use /init to generate one.".to_string()));
    }
    items.push(blank());

    // ── Footer hint ───────────────────────────────────────────────────────────
    items.push(dim(format!(
        "Edit ~/.config/forge/config.toml to change profile settings."
    )));
    items.push(blank());

    let list = List::new(items)
        .block(Block::default().style(Style::default().bg(Color::Rgb(8, 8, 14))));
    f.render_widget(list, area);
}
