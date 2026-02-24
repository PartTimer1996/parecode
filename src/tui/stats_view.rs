/// Stats tab — session totals, daily/all-time aggregates, per-task table.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem},
};

use super::AppState;
use crate::telemetry::Aggregate;

// ── Rendering helpers ─────────────────────────────────────────────────────────

/// Unicode block progress bar, width chars wide, value/max ratio filled.
/// Uses ░ for empty, gradient blocks for partial, █ for full.
fn bar(value: u64, max: u64, width: usize) -> String {
    if max == 0 {
        return "░".repeat(width);
    }
    let blocks = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    let filled_eighths = ((value as usize * width * 8) / max as usize).min(width * 8);
    let full = filled_eighths / 8;
    let partial = filled_eighths % 8;
    let mut s = "█".repeat(full);
    if partial > 0 && full < width {
        s.push_str(blocks[partial - 1]);
    }
    let empty = width.saturating_sub(s.chars().count());
    s.push_str(&"░".repeat(empty));
    s
}

fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn fmt_duration(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{}s", secs)
    } else {
        "—".to_string()
    }
}

fn cost_str(tokens_in: u32, tokens_out: u32, cost_per_mtok: Option<f64>) -> Option<String> {
    let cpm = cost_per_mtok?;
    // assume output is ~3× input price (rough Anthropic ratio)
    let cost = (tokens_in as f64 / 1_000_000.0) * cpm
        + (tokens_out as f64 / 1_000_000.0) * cpm * 3.0;
    if cost < 0.001 {
        Some("<$0.001".to_string())
    } else {
        Some(format!("${:.3}", cost))
    }
}

// ── Section builders ──────────────────────────────────────────────────────────

const BAR_W: usize = 16;

/// One stat row with an inline bar:
///   `  label    ████████░░░░░░░░  value_str`
fn stat_bar_row<'a>(
    label: &str,
    value: u64,
    max: u64,
    value_str: &str,
    bar_color: Color,
) -> ListItem<'a> {
    let b = bar(value, max, BAR_W);
    ListItem::new(Line::from(vec![
        Span::styled(format!("  {:<14}", label), Style::default().fg(Color::Rgb(100, 95, 140))),
        Span::styled(b, Style::default().fg(bar_color)),
        Span::styled(format!("  {}", value_str), Style::default().fg(Color::White)),
    ]))
}

fn kv_row<'a>(label: &str, value: &str) -> ListItem<'a> {
    ListItem::new(Line::from(vec![
        Span::styled(format!("  {:<14}", label), Style::default().fg(Color::Rgb(100, 95, 140))),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ]))
}

fn section_header<'a>(title: &str) -> ListItem<'a> {
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(title.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]))
}

fn divider<'a>() -> ListItem<'a> {
    ListItem::new(Line::raw(""))
}

/// Build items for a single Aggregate block (session / today / all-time).
fn aggregate_items<'a>(
    agg: &Aggregate,
    peak_context_pct: Option<u32>,
    budget_enforcements: Option<usize>,
    cost_per_mtok: Option<f64>,
    max_tokens: u64,   // for bar scaling
    max_tools: u64,
) -> Vec<ListItem<'a>> {
    let mut items = Vec::new();

    let total_tok = agg.total_tokens() as u64;
    let in_tok = agg.input_tokens as u64;
    let out_tok = agg.output_tokens as u64;
    let tools = agg.tool_calls as u64;
    let comp_pct = (agg.compression_ratio() * 100.0).round() as u64;

    // Tokens row
    let tok_str = format!(
        "{}  (in: {}  out: {})",
        fmt_k(total_tok), fmt_k(in_tok), fmt_k(out_tok)
    );
    items.push(stat_bar_row("tokens", total_tok, max_tokens.max(1), &tok_str, Color::Rgb(80, 140, 200)));

    // Tool calls row
    let avg_tools = if agg.tasks > 0 { tools / agg.tasks as u64 } else { 0 };
    let tools_str = format!("{}  (avg {}/task)", tools, avg_tools);
    items.push(stat_bar_row("tool calls", tools, max_tools.max(1), &tools_str, Color::Rgb(160, 120, 220)));

    // Compression row
    let comp_str = format!("{}%  ({}/{} compressed)", comp_pct, agg.compressed, agg.tool_calls);
    items.push(stat_bar_row("compression", comp_pct, 100, &comp_str, Color::Rgb(80, 180, 120)));

    // Budget enforcements (live only)
    if let Some(hits) = budget_enforcements {
        if hits > 0 {
            items.push(stat_bar_row("budget hits", hits as u64, hits.max(5) as u64, &hits.to_string(), Color::Rgb(220, 120, 40)));
        }
    }

    // Peak context (live only)
    if let Some(pct) = peak_context_pct {
        if pct > 0 {
            let peak_color = match pct {
                0..=50  => Color::Rgb(60, 180, 60),
                51..=75 => Color::Rgb(200, 180, 40),
                76..=90 => Color::Rgb(220, 120, 20),
                _       => Color::Rgb(220, 60, 60),
            };
            items.push(stat_bar_row("peak context", pct as u64, 100, &format!("{}%", pct), peak_color));
        }
    }

    // Duration
    if agg.duration_secs > 0 {
        let avg_dur = if agg.tasks > 0 { agg.duration_secs / agg.tasks as u32 } else { 0 };
        let dur_str = format!("{}  (avg {}/task)", fmt_duration(agg.duration_secs), fmt_duration(avg_dur));
        items.push(kv_row("time", &dur_str));
    }

    // Cost estimate
    if let Some(c) = cost_str(agg.input_tokens, agg.output_tokens, cost_per_mtok) {
        items.push(kv_row("est. cost", &c));
    }

    items
}

// ── Main draw ─────────────────────────────────────────────────────────────────

pub fn draw(f: &mut Frame, state: &AppState, area: Rect) {
    let mut all_items: Vec<ListItem<'static>> = Vec::new();

    let s = &state.stats;
    let cost = state.cost_per_mtok_input.map(|c| c as f64);

    // ── Section 0: In-Flight (shown only while agent is running) ──────────────
    if s.inflight_input_tokens > 0 || s.inflight_output_tokens > 0 {
        all_items.push(divider());
        all_items.push(section_header("▶ In-Flight"));
        all_items.push(kv_row("input",  &fmt_k(s.inflight_input_tokens as u64)));
        all_items.push(kv_row("output", &fmt_k(s.inflight_output_tokens as u64)));
        let total = s.inflight_input_tokens + s.inflight_output_tokens;
        all_items.push(kv_row("total",  &fmt_k(total as u64)));
        if let Some(cost_str) = cost_str(s.inflight_input_tokens, s.inflight_output_tokens, cost) {
            all_items.push(kv_row("est cost", &cost_str));
        }
    }

    // ── Section 1: This Session ───────────────────────────────────────────────
    all_items.push(divider());
    all_items.push(section_header("This Session"));

    if s.tasks_completed == 0 {
        all_items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("No tasks completed yet.", Style::default().fg(Color::DarkGray)),
        ])));
    } else {
        let session_agg = Aggregate {
            tasks: s.tasks_completed,
            input_tokens: s.total_input_tokens,
            output_tokens: s.total_output_tokens,
            tool_calls: s.total_tool_calls,
            compressed: s.total_compressed,
            duration_secs: s.records.iter().map(|r| r.duration_secs).sum(),
        };
        all_items.push(kv_row("tasks", &s.tasks_completed.to_string()));

        // Scale bars against the session max
        let max_tok = session_agg.total_tokens() as u64;
        let max_tools = session_agg.tool_calls as u64;

        all_items.extend(aggregate_items(
            &session_agg,
            Some(s.peak_context_pct),
            Some(s.budget_enforcements),
            cost,
            max_tok,
            max_tools,
        ));
    }
    all_items.push(divider());

    // ── Section 2: Today ──────────────────────────────────────────────────────
    let now = chrono::Utc::now().timestamp();
    let day_start = now - 86400;
    let today_records: Vec<_> = state.telemetry_history.iter()
        .filter(|r| r.timestamp >= day_start)
        .cloned()
        .collect();

    all_items.push(section_header("Today"));
    if today_records.is_empty() {
        all_items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("No tasks today.", Style::default().fg(Color::DarkGray)),
        ])));
    } else {
        let agg = Aggregate::from_records(&today_records);
        all_items.push(kv_row("tasks", &agg.tasks.to_string()));
        let max_tok = agg.total_tokens() as u64;
        let max_tools = agg.tool_calls as u64;
        all_items.extend(aggregate_items(&agg, None, None, cost, max_tok, max_tools));
    }
    all_items.push(divider());

    // ── Section 3: All Time ───────────────────────────────────────────────────
    all_items.push(section_header("All Time"));
    if state.telemetry_history.is_empty() {
        all_items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("No historical data.", Style::default().fg(Color::DarkGray)),
        ])));
    } else {
        let agg = Aggregate::from_records(&state.telemetry_history);
        all_items.push(kv_row("tasks", &agg.tasks.to_string()));
        let max_tok = agg.total_tokens() as u64;
        let max_tools = agg.tool_calls as u64;
        all_items.extend(aggregate_items(&agg, None, None, cost, max_tok, max_tools));

        // First-seen date
        if let Some(first) = state.telemetry_history.first() {
            let dt = chrono::DateTime::from_timestamp(first.timestamp, 0)
                .unwrap_or_default()
                .with_timezone(&chrono::Local);
            all_items.push(kv_row("since", &dt.format("%b %d %Y").to_string()));
        }
    }
    all_items.push(divider());

    // ── Section 4: Per-task table (this session) ──────────────────────────────
    if !s.records.is_empty() {
        all_items.push(section_header("Tasks — this session"));
        all_items.push(ListItem::new(Line::from(vec![
            Span::styled("   #  ", Style::default().fg(Color::Rgb(60, 57, 90))),
            Span::styled(format!("{:<9}", "in tok"),   Style::default().fg(Color::Rgb(60, 57, 90))),
            Span::styled(format!("{:<9}", "out tok"),  Style::default().fg(Color::Rgb(60, 57, 90))),
            Span::styled(format!("{:<7}", "tools"),    Style::default().fg(Color::Rgb(60, 57, 90))),
            Span::styled(format!("{:<7}", "time"),     Style::default().fg(Color::Rgb(60, 57, 90))),
            Span::styled("task",                        Style::default().fg(Color::Rgb(60, 57, 90))),
        ])));

        let max_tok_rec = s.records.iter()
            .map(|r| (r.input_tokens + r.output_tokens) as u64)
            .max()
            .unwrap_or(1);

        for (i, rec) in s.records.iter().enumerate() {
            let total = (rec.input_tokens + rec.output_tokens) as u64;
            let b = bar(total, max_tok_rec, 8);
            let preview: String = rec.task_preview.chars().take(32).collect();
            all_items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("  {:>2}  ", i + 1),            Style::default().fg(Color::Rgb(100, 95, 140))),
                Span::styled(format!("{:<9}", fmt_k(rec.input_tokens as u64)),  Style::default().fg(Color::Rgb(100, 180, 255))),
                Span::styled(format!("{:<9}", fmt_k(rec.output_tokens as u64)), Style::default().fg(Color::Rgb(100, 220, 180))),
                Span::styled(format!("{:<7}", rec.tool_calls),        Style::default().fg(Color::Rgb(200, 160, 255))),
                Span::styled(format!("{:<7}", fmt_duration(rec.duration_secs)), Style::default().fg(Color::Rgb(160, 150, 190))),
                Span::styled(b,                                        Style::default().fg(Color::Rgb(70, 120, 170))),
                Span::styled(format!("  {}", preview),                Style::default().fg(Color::Rgb(150, 145, 185))),
            ])));
        }
        all_items.push(divider());
    }

    // Footer hint
    all_items.push(ListItem::new(Line::from(vec![
        Span::styled("  /stats reset  to clear all history", Style::default().fg(Color::Rgb(55, 52, 80))),
    ])));
    all_items.push(divider());

    // Apply scroll offset
    let scroll = state.stats_scroll;
    let visible: Vec<ListItem<'static>> = all_items.into_iter().skip(scroll).collect();

    let list = List::new(visible)
        .block(Block::default().style(Style::default().bg(Color::Rgb(8, 8, 14))));
    f.render_widget(list, area);
}
