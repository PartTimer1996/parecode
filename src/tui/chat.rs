/// Chat history pane rendering — build_items, draw_history, draw_chips, spinner, utilities.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph},
};

use super::{AppState, ConversationEntry, Mode};
use crate::plan::{PlanStatus, StepStatus};
use crate::ui::tool_glyph;

// ── Spinner ────────────────────────────────────────────────────────────────────

pub const SPINNER_GLYPHS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_MSGS: &[(&str, Color)] = &[
    ("thinking…",         Color::Cyan),
    ("reasoning…",        Color::Cyan),
    ("reading context…",  Color::Cyan),
    ("crafting response…",Color::Rgb(0, 200, 255)),
    ("working on it…",    Color::Rgb(0, 220, 180)),
    ("almost there…",     Color::Rgb(100, 200, 255)),
    ("processing…",       Color::Cyan),
    ("analysing…",        Color::Cyan),
    ("on it…",            Color::Rgb(0, 220, 180)),
    ("running tools…",    Color::Yellow),
];

pub fn spinner_frame(tick: u32) -> (&'static str, &'static str, Color) {
    let glyph = SPINNER_GLYPHS[(tick as usize) % SPINNER_GLYPHS.len()];
    // Message cycles more slowly — changes every ~2 seconds (120ms × 16 ticks)
    let msg_idx = (tick as usize / 16) % SPINNER_MSGS.len();
    let (msg, color) = SPINNER_MSGS[msg_idx];
    (glyph, msg, color)
}

// ── Tool colour ────────────────────────────────────────────────────────────────

fn tool_color(tool_name: &str) -> Color {
    match tool_name {
        "read_file"               => Color::Cyan,
        "write_file" | "edit_file" => Color::Green,
        "bash"                    => Color::Yellow,
        "search"                  => Color::Magenta,
        "list_files"              => Color::Blue,
        _                         => Color::White,
    }
}

// ── History items builder ──────────────────────────────────────────────────────

pub fn build_items(state: &AppState, term_width: u16) -> Vec<ListItem<'static>> {
    let mut items: Vec<ListItem<'static>> = Vec::new();

    for entry in &state.entries {
        match entry {
            ConversationEntry::UserMessage(msg) => {
                let msg = msg.clone();
                // Bubble colours
                let bg       = Color::Rgb(28, 26, 52);
                let border   = Color::Rgb(110, 90, 200);
                let label_fg = Color::Rgb(160, 140, 255);
                let text_fg  = Color::Rgb(235, 232, 255);
                let body_style = Style::default().fg(text_fg).bg(bg);
                let edge_style = Style::default().fg(border).bg(bg);

                // Dynamic widths — 2 chars left margin, 1 right margin
                let inner_w = (term_width as usize).saturating_sub(3).max(10);
                // Top: "╭─ you ──...──╮"  — label is " you " (5 chars), corners+space = 4
                let dash_total = inner_w.saturating_sub(4 + 5); // "╭─ " + "you" + " " + "╮"
                let top_dashes = "─".repeat(dash_total);
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("╭─ "), edge_style),
                    Span::styled("you", Style::default()
                        .fg(label_fg).bg(bg).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {top_dashes}╮"), edge_style),
                ])));

                // Body — word-wrap inside the box (inner_w minus "│ " = 2)
                let wrap_width = inner_w.saturating_sub(2).max(10);
                let raw_lines: Vec<&str> = if msg.is_empty() { vec![""] } else { msg.lines().collect() };
                let wrapped: Vec<String> = raw_lines
                    .iter()
                    .flat_map(|line| wrap_text(line, wrap_width))
                    .collect();
                for line in &wrapped {
                    items.push(ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("│ ", edge_style),
                        Span::styled(line.clone(), body_style),
                    ])));
                }

                // Bottom: "╰──...──╯"
                let bot_dashes = "─".repeat(inner_w.saturating_sub(2)); // "╰" + dashes + "╯"
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("╰{bot_dashes}╯"), edge_style),
                ])));
                items.push(ListItem::new(Line::raw("")));
            }

            ConversationEntry::ThinkingChunk(text) => {
                // Render model reasoning dimmed and indented — visually distinct from response
                let wrap_width = (term_width as usize).saturating_sub(10).max(20);
                let think_fg = Color::Rgb(100, 100, 130); // muted purple-grey
                let mut first = true;
                for src_line in text.lines() {
                    for w in wrap_text(src_line, wrap_width) {
                        if first {
                            first = false;
                            items.push(ListItem::new(Line::from(vec![
                                Span::raw("  "),
                                Span::styled("think ", Style::default().fg(think_fg).add_modifier(Modifier::ITALIC)),
                                Span::styled(w, Style::default().fg(think_fg).add_modifier(Modifier::ITALIC)),
                            ])));
                        } else {
                            items.push(ListItem::new(Line::from(vec![
                                Span::raw("        "),
                                Span::styled(w, Style::default().fg(think_fg).add_modifier(Modifier::ITALIC)),
                            ])));
                        }
                    }
                }
                items.push(ListItem::new(Line::raw("")));
            }

            ConversationEntry::AssistantChunk(text) => {
                // Word-wrap each source line to terminal width
                // "        " indent = 8 cols
                let wrap_width = (term_width as usize).saturating_sub(8).max(20);
                let label_fg = Color::Rgb(0, 210, 210);
                let text_fg  = Color::Rgb(210, 230, 255);

                let mut first = true;
                for src_line in text.lines() {
                    let wrapped = wrap_text(src_line, wrap_width);
                    for w in wrapped {
                        if first {
                            first = false;
                            items.push(ListItem::new(Line::from(vec![
                                Span::raw("  "),
                                Span::styled("forge", Style::default()
                                    .fg(label_fg)
                                    .add_modifier(Modifier::BOLD)),
                                Span::styled("  ", Style::default()),
                                Span::styled(w, Style::default().fg(text_fg)),
                            ])));
                        } else {
                            items.push(ListItem::new(Line::from(vec![
                                Span::raw("        "),
                                Span::styled(w, Style::default().fg(text_fg)),
                            ])));
                        }
                    }
                }
                // Blank source lines (empty lines between paragraphs)
                // are preserved as blank items via wrap_text returning [""]
                items.push(ListItem::new(Line::raw("")));
            }

            ConversationEntry::ToolCall { name, args_summary } => {
                let glyph = tool_glyph(name).to_string();
                let color = tool_color(name);
                let name = name.clone();
                let args_summary = args_summary.clone();
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{glyph} {name} "), Style::default().fg(color)),
                    Span::styled(args_summary, Style::default().fg(Color::DarkGray)),
                ])));
            }

            ConversationEntry::ToolResult(summary) => {
                let mut line_iter = summary.lines();
                // First line gets the "→ " prefix
                if let Some(first) = line_iter.next() {
                    let color = if first.starts_with('✗') || first.contains("failed") || first.contains("error") {
                        Color::Red
                    } else {
                        Color::DarkGray
                    };
                    items.push(ListItem::new(Line::from(vec![
                        Span::raw("    "),
                        Span::styled("→ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(first.to_string(), Style::default().fg(color)),
                    ])));
                    // Subsequent lines indented to align with first, up to 20 lines
                    for line in line_iter.take(20) {
                        let color = if line.contains("error[") || line.starts_with("error") {
                            Color::Red
                        } else if line.contains("warning") {
                            Color::Yellow
                        } else {
                            Color::DarkGray
                        };
                        items.push(ListItem::new(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(line.to_string(), Style::default().fg(color)),
                        ])));
                    }
                }
            }

            ConversationEntry::CacheHit(path) => {
                let path = path.clone();
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("↩ cache  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(path, Style::default().fg(Color::DarkGray)),
                ])));
            }

            ConversationEntry::SystemMsg(msg) => {
                for line in msg.lines() {
                    let line = line.to_string();
                    items.push(ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(line, Style::default().fg(Color::DarkGray)),
                    ])));
                }
            }

            ConversationEntry::PlanCard => {
                items.extend(build_plan_card_items(state, state.cost_per_mtok_input));
            }

            ConversationEntry::TaskComplete {
                input_tokens,
                output_tokens,
                tool_calls,
                compressed_count,
            } => {
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled("✓ done", Style::default()
                        .fg(Color::Rgb(0, 240, 120))
                        .add_modifier(Modifier::BOLD)),
                    Span::styled("  ·  ", Style::default().fg(Color::Rgb(50, 50, 70))),
                    Span::styled("in ", Style::default().fg(Color::DarkGray)),
                    Span::styled(input_tokens.to_string(), Style::default().fg(Color::Rgb(100, 180, 255))),
                    Span::styled("  out ", Style::default().fg(Color::DarkGray)),
                    Span::styled(output_tokens.to_string(), Style::default().fg(Color::Rgb(100, 220, 180))),
                    Span::styled("  tools ", Style::default().fg(Color::DarkGray)),
                    Span::styled(tool_calls.to_string(), Style::default().fg(Color::Rgb(200, 160, 255))),
                ];
                if *compressed_count > 0 {
                    spans.push(Span::styled(
                        format!("  · {compressed_count} compressed"),
                        Style::default().fg(Color::Rgb(80, 80, 100)),
                    ));
                }
                items.push(ListItem::new(Line::from(spans)));
                items.push(ListItem::new(Line::raw("")));
            }

            ConversationEntry::HookOutput { event, output, success } => {
                let (mark, color) = if *success {
                    ("✓", Color::Rgb(60, 60, 80))
                } else {
                    ("✗", Color::Rgb(200, 140, 60))
                };
                let label = format!("  ⚙ {event} {mark}");
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(label, Style::default().fg(color)),
                ])));
                for line in output.lines().take(10) {
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("    {line}"),
                            Style::default().fg(color),
                        ),
                    ])));
                }
            }
        }
    }

    if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
        let glyph = SPINNER_GLYPHS[(state.spinner_tick as usize) % SPINNER_GLYPHS.len()];
        let live = state.last_stream_text.trim();
        // Show last line of streamed text (strip newlines, truncate to fit)
        let display_text: String = live
            .lines()
            .last()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        if display_text.is_empty() {
            // Nothing streamed yet — fall back to rotating status message
            let (_, msg, color) = spinner_frame(state.spinner_tick);
            items.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(msg.to_string(), Style::default().fg(color)),
            ])));
        } else if state.stream_in_think {
            items.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(Color::Rgb(120, 100, 180)).add_modifier(Modifier::BOLD)),
                Span::styled("think  ", Style::default().fg(Color::Rgb(80, 70, 130))),
                Span::styled(display_text, Style::default().fg(Color::Rgb(130, 115, 170)).add_modifier(Modifier::ITALIC | Modifier::DIM)),
            ])));
        } else {
            items.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{glyph} "), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(display_text, Style::default().fg(Color::Rgb(180, 220, 255))),
            ])));
        }
    }

    items
}

// ── Plan card ─────────────────────────────────────────────────────────────────

pub fn build_plan_card_items(state: &AppState, cost_per_mtok: Option<f64>) -> Vec<ListItem<'static>> {
    let Some(pr) = &state.plan_review else { return vec![] };
    let plan = &pr.plan;
    let running = matches!(state.mode, Mode::PlanRunning);
    let complete = plan.status == PlanStatus::Complete;

    let (header_fg, header_label) = if complete {
        (Color::Rgb(0, 200, 100), "✓ plan complete")
    } else if running {
        (Color::Cyan, "▶ running plan")
    } else {
        (Color::Rgb(220, 160, 30), "◇ plan ready")
    };

    let mut out: Vec<ListItem<'static>> = Vec::new();

    // Header line
    let task_fg = Color::Rgb(230, 220, 255);
    out.push(ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{header_label}  "),
            Style::default().fg(header_fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(plan.task.clone(), Style::default().fg(task_fg)),
    ])));

    // Cost estimate line (only in review mode before execution)
    if !running && !complete {
        let estimate = plan.estimate_display(cost_per_mtok);
        let step_count = plan.steps.len();
        out.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{step_count} step{}  ·  {estimate}", if step_count == 1 { "" } else { "s" }),
                Style::default().fg(Color::Rgb(80, 75, 100)),
            ),
        ])));
    }

    // Divider
    out.push(ListItem::new(Line::from(vec![
        Span::styled(
            "  ─────────────────────────────────────",
            Style::default().fg(Color::Rgb(50, 50, 70)),
        ),
    ])));

    // Steps
    for (i, step) in plan.steps.iter().enumerate() {
        let selected = !running && i == pr.selected;
        let is_running_step = running && i == state.plan_running_step;

        let (status_glyph, status_color) = match step.status {
            StepStatus::Pass     => ("✓", Color::Rgb(0, 200, 100)),
            StepStatus::Approved => ("✓", Color::Rgb(200, 160, 30)),  // amber — reviewed, not yet run
            StepStatus::Fail     => ("✗", Color::Rgb(220, 60, 60)),
            StepStatus::Running  => {
                let g = SPINNER_GLYPHS[(state.spinner_tick as usize) % SPINNER_GLYPHS.len()];
                (g, Color::Cyan)
            }
            StepStatus::Skipped  => ("–", Color::DarkGray),
            StepStatus::Pending  => ("○", Color::DarkGray),
        };

        let ann_mark = if step.user_annotation.is_some() { " ✎" } else { "" };

        let (num_fg, desc_fg, bg) = if selected {
            (Color::White, Color::White, Color::Rgb(35, 30, 18))
        } else if is_running_step {
            (Color::White, Color::White, Color::Rgb(15, 30, 45))
        } else {
            (Color::DarkGray, Color::Rgb(180, 180, 200), Color::Reset)
        };

        out.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {status_glyph} "),
                Style::default().fg(status_color).bg(bg),
            ),
            Span::styled(
                format!("{:>2}  ", i + 1),
                Style::default().fg(num_fg).bg(bg),
            ),
            Span::styled(
                format!("{}{}", step.description, ann_mark),
                Style::default().fg(desc_fg).bg(bg),
            ),
        ])));

        // Show annotation inline if selected and annotating
        if selected && pr.annotating {
            out.push(ListItem::new(Line::from(vec![
                Span::raw("       "),
                Span::styled("note: ", Style::default().fg(Color::Rgb(200, 160, 0))),
                Span::styled(pr.annotation_input.clone(), Style::default().fg(Color::White)),
                Span::styled("█", Style::default().fg(Color::Rgb(200, 160, 0))),
            ])));
        } else if selected {
            if let Some(note) = &step.user_annotation {
                out.push(ListItem::new(Line::from(vec![
                    Span::raw("       "),
                    Span::styled("note: ", Style::default().fg(Color::Rgb(160, 120, 0))),
                    Span::styled(note.clone(), Style::default().fg(Color::Rgb(200, 170, 100))),
                ])));
            }
        }
    }

    // Footer hint (only in review mode, not running, not complete)
    if !running && !complete {
        // Check if all steps approved so we can show the "ready to run" prompt
        let all_approved = plan.steps.iter().all(|s| {
            matches!(s.status, StepStatus::Approved | StepStatus::Pass)
        });
        out.push(ListItem::new(Line::from(vec![
            Span::styled(
                "  ─────────────────────────────────────",
                Style::default().fg(Color::Rgb(50, 50, 70)),
            ),
        ])));
        let hint_fg = Color::Rgb(80, 75, 50);
        let key_fg = Color::Rgb(200, 160, 30);
        if all_approved {
            // All steps reviewed — show the run prompt prominently
            out.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("all steps approved  ", Style::default().fg(Color::Rgb(200, 160, 30))),
                Span::styled("Enter", Style::default().fg(Color::Rgb(0, 220, 120)).add_modifier(Modifier::BOLD)),
                Span::styled(" to run  ", Style::default().fg(hint_fg)),
                Span::styled("Esc", Style::default().fg(key_fg)),
                Span::styled(" cancel", Style::default().fg(hint_fg)),
            ])));
        } else {
            out.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("↑↓", Style::default().fg(key_fg)),
                Span::styled(" navigate  ", Style::default().fg(hint_fg)),
                Span::styled("a", Style::default().fg(key_fg)),
                Span::styled(" approve step  ", Style::default().fg(hint_fg)),
                Span::styled("e", Style::default().fg(key_fg)),
                Span::styled(" annotate  ", Style::default().fg(hint_fg)),
                Span::styled("Esc", Style::default().fg(key_fg)),
                Span::styled(" cancel", Style::default().fg(hint_fg)),
            ])));
        }
    }

    out.push(ListItem::new(Line::raw("")));
    out
}

// ── Draw functions ─────────────────────────────────────────────────────────────

pub fn draw_history(f: &mut Frame, state: &AppState, area: Rect) {
    let all_items = build_items(state, area.width);
    let total = all_items.len();
    let visible = area.height as usize;

    let skip = if total > visible {
        (total - visible).saturating_sub(state.scroll)
    } else {
        0
    };

    let sliced: Vec<ListItem<'static>> = all_items.into_iter().skip(skip).collect();
    let list = List::new(sliced)
        .block(Block::default().style(Style::default().bg(Color::Rgb(8, 8, 14))));
    f.render_widget(list, area);
}

pub fn draw_chips(f: &mut Frame, state: &AppState, area: Rect) {
    let mut spans = vec![Span::styled(" 📎 ", Style::default().fg(Color::DarkGray))];
    for (i, file) in state.attached_files.iter().enumerate() {
        let focused = state.focused_chip == Some(i);
        let name = short_filename(&file.path);
        let (bg, fg) = if focused {
            (Color::Cyan, Color::Black)
        } else {
            (Color::DarkGray, Color::White)
        };
        spans.push(Span::styled(
            format!(" {name} ✕ "),
            Style::default().fg(fg).bg(bg),
        ));
        spans.push(Span::raw(" "));
    }
    if !state.attached_files.is_empty() {
        spans.push(Span::styled(
            " Tab to focus · Del to remove ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn short_filename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path)
}

// ── Utilities ──────────────────────────────────────────────────────────────────

/// Format "used/total" — shows raw tokens below 1k, switches to `k` above.
/// e.g. "340/32k", "1.2k/32k", "12k/32k"
pub fn fmt_tokens(used: usize, total: u32) -> String {
    let total_k = total / 1000;
    let used_fmt = if used < 1000 {
        format!("{used}")
    } else if used < 10_000 {
        format!("{:.1}k", used as f32 / 1000.0)
    } else {
        format!("{}k", used / 1000)
    };
    format!("{used_fmt}/{total_k}k")
}

/// Word-wrap a single line of text to `max_width` columns.
/// Splits on whitespace; never truncates mid-word unless the word alone exceeds max_width.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split_whitespace() {
        let word_width = word.len(); // close enough for ASCII; unicode_width would be better
        if current_width == 0 {
            // First word on line
            current.push_str(word);
            current_width = word_width;
        } else if current_width + 1 + word_width <= max_width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(current.clone());
            current = word.to_string();
            current_width = word_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

pub fn truncate_path(path: &str, max: usize) -> String {
    if path.len() <= max {
        path.to_string()
    } else {
        format!("…{}", &path[path.len() - max + 1..])
    }
}
