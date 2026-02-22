/// Ratatui draw functions for Forge.
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap}, // Clear used by file picker / palette / session browser
};

use super::{AppState, ConversationEntry, FilePickerState, Mode, SessionBrowserState, cwd_str};
use crate::plan::{PlanStatus, StepStatus};
use chrono::TimeZone as _;
use crate::ui::tool_glyph;

// ── Splash screen ─────────────────────────────────────────────────────────────

const LOGO: &str = r#"
  ███████╗ ██████╗ ██████╗  ██████╗ ███████╗
  ██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝
  █████╗  ██║   ██║██████╔╝██║  ███╗█████╗
  ██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝
  ██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗
  ╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝
"#;

pub fn draw_splash(f: &mut Frame) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );

    let logo_lines: Vec<Line> = LOGO
        .lines()
        .enumerate()
        .map(|(i, line)| {
            // Cycle through a gradient: dark cyan → cyan → white → cyan
            let color = match i % 6 {
                0 => Color::DarkGray,
                1 | 5 => Color::Cyan,
                2 | 4 => Color::Rgb(0, 220, 220),
                _ => Color::White,
            };
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    let logo_height = logo_lines.len() as u16;
    let y = area.height.saturating_sub(logo_height + 4) / 2;

    let logo_area = Rect {
        x: area.x,
        y: area.y + y,
        width: area.width,
        height: logo_height,
    };

    let subtitle_area = Rect {
        x: area.x,
        y: logo_area.y + logo_height + 1,
        width: area.width,
        height: 1,
    };

    let hint_area = Rect {
        x: area.x,
        y: subtitle_area.y + 2,
        width: area.width,
        height: 1,
    };

    f.render_widget(
        Paragraph::new(logo_lines).alignment(Alignment::Center),
        logo_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("hyper-efficient coding agent", Style::default().fg(Color::DarkGray)),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled("local & cloud LLMs", Style::default().fg(Color::DarkGray)),
        ])).alignment(Alignment::Center),
        subtitle_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(
            Span::styled("loading…", Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)),
        )).alignment(Alignment::Center),
        hint_area,
    );
}

// ── Tool colours ──────────────────────────────────────────────────────────────

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

// ── Main draw entry point ─────────────────────────────────────────────────────

pub fn draw(f: &mut Frame, state: &AppState) {
    let area = f.area();

    let has_chips = !state.attached_files.is_empty();
    let constraints = if has_chips {
        vec![
            Constraint::Min(0),     // history
            Constraint::Length(1),  // status bar
            Constraint::Length(1),  // stats bar
            Constraint::Length(1),  // attached files chips row
            Constraint::Length(3),  // input box
        ]
    } else {
        vec![
            Constraint::Min(0),     // history
            Constraint::Length(1),  // status bar
            Constraint::Length(1),  // stats bar
            Constraint::Length(3),  // input box
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_history(f, state, chunks[0]);
    draw_status_bar(f, state, chunks[1]);
    draw_stats_bar(f, state, chunks[2]);
    if has_chips {
        draw_chips(f, state, chunks[3]);
        draw_input(f, state, chunks[4]);
    } else {
        draw_input(f, state, chunks[3]);
    }

    if state.mode == Mode::Palette {
        draw_palette(f, state, area);
    }
    if state.mode == Mode::FilePicker {
        if let Some(fp) = &state.file_picker {
            draw_file_picker(f, fp, area);
        }
    }
    if state.mode == Mode::SessionBrowser {
        if let Some(browser) = &state.session_browser {
            draw_session_browser(f, browser, area);
        }
    }
    // Plan review is now inline in history (PlanCard entry) — no overlay needed
}

// ── History pane ──────────────────────────────────────────────────────────────

fn build_items(state: &AppState, term_width: u16) -> Vec<ListItem<'static>> {
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
                let first = summary.lines().next().unwrap_or(summary.as_str()).to_string();
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("→ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(first, Style::default().fg(Color::DarkGray)),
                ])));
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
        }
    }

    if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
        let (glyph, msg, color) = spinner_frame(state.spinner_tick);
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{glyph} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(msg.to_string(), Style::default().fg(color)),
        ])));
    }

    items
}

fn build_plan_card_items(state: &AppState, cost_per_mtok: Option<f64>) -> Vec<ListItem<'static>> {
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

// ── Spinner ───────────────────────────────────────────────────────────────────

const SPINNER_GLYPHS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
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

fn spinner_frame(tick: u32) -> (&'static str, &'static str, Color) {
    let glyph = SPINNER_GLYPHS[(tick as usize) % SPINNER_GLYPHS.len()];
    // Message cycles more slowly — changes every ~2 seconds (120ms × 16 ticks)
    let msg_idx = (tick as usize / 16) % SPINNER_MSGS.len();
    let (msg, color) = SPINNER_MSGS[msg_idx];
    (glyph, msg, color)
}

fn draw_history(f: &mut Frame, state: &AppState, area: Rect) {
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

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_status_bar(f: &mut Frame, state: &AppState, area: Rect) {
    let pct = if state.context_tokens > 0 {
        (state.ctx_used as f32 / state.context_tokens as f32 * 100.0) as u32
    } else {
        0
    };
    // Context bar fill — mini progress bar using block chars
    let bar_width = 8usize;
    let filled = ((pct as usize).min(100) * bar_width / 100).min(bar_width);
    let ctx_bar: String = format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(bar_width - filled)
    );
    let ctx_color = match pct {
        0..=50  => Color::Green,
        51..=75 => Color::Yellow,
        76..=90 => Color::Rgb(255, 140, 0),
        _       => Color::Red,
    };

    let cwd = cwd_str();
    let cwd_short = truncate_path(&cwd, 28);
    let compress_note = if state.ctx_compressed { " ⟳" } else { "" };

    // Session memory indicator
    let turn_count = state.conversation_turns.len();

    // Plan progress indicator
    let plan_indicator = if let Some(pr) = &state.plan_review {
        let total = pr.plan.steps.len();
        let done = pr.plan.steps.iter().filter(|s| s.status == StepStatus::Pass).count();
        let failed = pr.plan.steps.iter().any(|s| s.status == StepStatus::Fail);
        if pr.plan.status == PlanStatus::Complete {
            format!("  ✓ plan {total}/{total}")
        } else if failed {
            format!("  ✗ plan {done}/{total}")
        } else if matches!(state.mode, Mode::PlanRunning | Mode::PlanReview) {
            format!("  ◇ step {}/{total}", state.plan_running_step + 1)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Animated spinner glyph in status bar when running
    let (status_glyph, status_color) = if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
        let g = SPINNER_GLYPHS[(state.spinner_tick as usize) % SPINNER_GLYPHS.len()];
        (g, Color::Cyan)
    } else {
        ("▲", Color::White)
    };

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(status_glyph, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled(" forge", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(state.profile.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(state.model.clone(), Style::default().fg(Color::Rgb(100, 180, 220))),
        Span::styled("  ", Style::default()),
        Span::styled(cwd_short, Style::default().fg(Color::DarkGray)),
        Span::styled("  ", Style::default()),
        Span::styled(ctx_bar, Style::default().fg(ctx_color)),
        Span::styled(
            format!(" {pct}%"),
            Style::default().fg(ctx_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}{compress_note}", fmt_tokens(state.ctx_used, state.context_tokens)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("  ", Style::default()),
        Span::styled("◈", Style::default().fg(Color::Rgb(80, 70, 140))),
        Span::styled(
            format!(" {turn_count}"),
            Style::default().fg(if turn_count > 0 {
                Color::Rgb(140, 120, 220)
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            if state.session_resumed { "↩" } else { "" },
            Style::default().fg(Color::Rgb(100, 90, 180)),
        ),
        Span::styled(
            "  Ctrl+H history",
            Style::default().fg(Color::Rgb(55, 50, 90)),
        ),
        Span::styled(
            plan_indicator,
            Style::default().fg(Color::Rgb(200, 160, 50)).add_modifier(Modifier::BOLD),
        ),
    ]);

    let bar_style = if state.mode == Mode::AgentRunning {
        Style::default().bg(Color::Rgb(15, 15, 25))
    } else {
        Style::default().bg(Color::Rgb(10, 10, 18))
    };

    f.render_widget(
        Paragraph::new(line).style(bar_style),
        area,
    );
}

// ── Stats bar (always-visible session telemetry) ──────────────────────────────

fn draw_stats_bar(f: &mut Frame, state: &AppState, area: Rect) {
    let s = &state.stats;

    // Only show meaningful data once at least one task has run
    let (task_str, token_str, tool_str, ratio_str) = if s.tasks_completed == 0 {
        (
            String::from("no tasks yet"),
            String::new(),
            String::new(),
            String::new(),
        )
    } else {
        let total_tok = s.total_tokens();
        let avg_tok = s.avg_tokens_per_task();
        let ratio = s.compression_ratio();

        (
            format!("{} task{}", s.tasks_completed, if s.tasks_completed == 1 { "" } else { "s" }),
            format!("  {}tok  avg {}/task", fmt_k(total_tok), fmt_k(avg_tok)),
            format!("  {} tool calls", s.total_tool_calls),
            if s.total_tool_calls > 0 {
                format!("  {:.0}% compressed", ratio * 100.0)
            } else {
                String::new()
            },
        )
    };

    // Peak context colour
    let peak_color = match s.peak_context_pct {
        0..=50  => Color::DarkGray,
        51..=75 => Color::Rgb(140, 120, 40),
        76..=90 => Color::Rgb(180, 100, 20),
        _       => Color::Rgb(180, 60, 60),
    };
    let peak_str = if s.peak_context_pct > 0 {
        format!("  peak {}%", s.peak_context_pct)
    } else {
        String::new()
    };

    let budget_str = if s.budget_enforcements > 0 {
        format!("  {} compressions", s.budget_enforcements)
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled("  ∑ ", Style::default().fg(Color::Rgb(60, 55, 100))),
        Span::styled(task_str, Style::default().fg(Color::Rgb(120, 110, 180))),
        Span::styled(token_str, Style::default().fg(Color::Rgb(80, 80, 120))),
        Span::styled(tool_str, Style::default().fg(Color::Rgb(70, 70, 110))),
        Span::styled(ratio_str, Style::default().fg(Color::Rgb(60, 100, 80))),
        Span::styled(peak_str, Style::default().fg(peak_color)),
        Span::styled(budget_str, Style::default().fg(Color::Rgb(80, 70, 60))),
    ]);

    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(7, 7, 14))),
        area,
    );
}

fn fmt_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

// ── Attached files chips row ──────────────────────────────────────────────────

fn draw_chips(f: &mut Frame, state: &AppState, area: Rect) {
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

// ── Input box ─────────────────────────────────────────────────────────────────

fn draw_input(f: &mut Frame, state: &AppState, area: Rect) {
    let (border_color, prompt_color, prompt_char) = match state.mode {
        Mode::AgentRunning   => (Color::Rgb(40, 40, 60),  Color::DarkGray,           "·"),
        Mode::Palette        => (Color::Cyan,              Color::Cyan,               "⌘"),
        Mode::FilePicker     => (Color::Green,             Color::Green,              "@"),
        Mode::SessionBrowser => (Color::Rgb(110, 90, 200), Color::Rgb(110, 90, 200), "◈"),
        Mode::PlanReview     => (Color::Rgb(200, 140, 0),  Color::Rgb(220, 160, 0),  "◇"),
        Mode::PlanRunning    => (Color::Rgb(40, 40, 60),   Color::DarkGray,          "▶"),
        Mode::Normal         => (Color::Rgb(60, 60, 80),  Color::Cyan,               "❯"),
    };

    let prompt_span = Span::styled(
        format!("  {prompt_char} "),
        Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
    );

    let input_text = if state.mode == Mode::Palette {
        state.palette_query.clone()
    } else {
        state.input.clone()
    };

    let content_span = if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
        let tick = state.spinner_tick as usize;
        let cancel_hints = ["Ctrl+C to cancel", "Ctrl+C to interrupt", "Ctrl+C to stop"];
        let hint = cancel_hints[(tick / 20) % cancel_hints.len()];
        Span::styled(hint.to_string(), Style::default().fg(Color::Rgb(60, 60, 80)))
    } else if state.mode == Mode::FilePicker {
        Span::raw(input_text.clone())
    } else if state.mode == Mode::PlanReview {
        // Dynamic hint based on whether all steps are approved
        let all_approved = state.plan_review.as_ref().map(|pr| {
            pr.plan.steps.iter().all(|s| matches!(s.status, StepStatus::Approved | StepStatus::Pass))
        }).unwrap_or(false);
        if all_approved {
            Span::styled(
                "Enter to run plan  ·  Esc cancel",
                Style::default().fg(Color::Rgb(0, 180, 80)),
            )
        } else {
            Span::styled(
                "↑↓ navigate  a approve step  e annotate  Enter run when all approved  Esc cancel",
                Style::default().fg(Color::Rgb(100, 80, 30)),
            )
        }
    } else if input_text.is_empty() {
        if state.mode == Mode::Palette {
            Span::styled("search commands…", Style::default().fg(Color::Rgb(70, 70, 90)))
        } else {
            Span::styled(
                "message · @ attach · Ctrl+H history · Ctrl+P commands",
                Style::default().fg(Color::Rgb(70, 70, 90)),
            )
        }
    } else {
        Span::styled(input_text.clone(), Style::default().fg(Color::White))
    };

    let input_line = Line::from(vec![prompt_span, content_span]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(Color::Rgb(8, 8, 14)));

    let paragraph = Paragraph::new(input_line)
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, area);

    // Position cursor at the actual edit cursor, not end of string
    if matches!(state.mode, Mode::Normal | Mode::Palette | Mode::FilePicker | Mode::PlanReview) {
        use unicode_width::UnicodeWidthStr;
        // prompt is "  ❯ " — ❯ is 1 wide, total visible width is 4 cols
        let prompt_width: u16 = 4;
        let (text, cursor_byte) = if state.mode == Mode::Palette {
            (state.palette_query.as_str(), state.palette_query.len())
        } else {
            (state.input.as_str(), state.cursor)
        };
        // Width of text up to the cursor byte position
        let text_before_cursor = &text[..cursor_byte.min(text.len())];
        let cursor_x = area.x + prompt_width + text_before_cursor.width() as u16;
        let cursor_y = area.y + 1; // +1 for top border
        if cursor_x < area.x + area.width {
            f.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

// ── Command palette overlay ───────────────────────────────────────────────────

fn draw_palette(f: &mut Frame, state: &AppState, area: Rect) {
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

// ── File picker overlay ───────────────────────────────────────────────────────

fn draw_file_picker(f: &mut Frame, fp: &FilePickerState, area: Rect) {
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

// ── Session browser overlay ───────────────────────────────────────────────────

fn draw_session_browser(f: &mut Frame, browser: &SessionBrowserState, area: Rect) {
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


// ── Utility ───────────────────────────────────────────────────────────────────

/// Format "used/total" — shows raw tokens below 1k, switches to `k` above.
/// e.g. "340/32k", "1.2k/32k", "12k/32k"
fn fmt_tokens(used: usize, total: u32) -> String {
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
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
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

fn truncate_path(path: &str, max: usize) -> String {
    if path.len() <= max {
        path.to_string()
    } else {
        format!("…{}", &path[path.len() - max + 1..])
    }
}
