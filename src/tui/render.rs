/// Ratatui draw entry-point for PareCode.
/// Thin dispatcher — most rendering lives in chat.rs and overlays.rs.
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::{AppState, Mode, Tab, cwd_str};
use super::chat::{SPINNER_GLYPHS, fmt_tokens, truncate_path};
use crate::plan::StepStatus;

// ── Splash screen ─────────────────────────────────────────────────────────────

const LOGO: &str = r#"
  ██████╗  █████╗ ██████╗ ███████╗ ██████╗ ██████╗ ██████╗ ███████╗
  ██╔══██╗██╔══██╗██╔══██╗██╔════╝██╔════╝██╔═══██╗██╔══██╗██╔════╝
  ██████╔╝███████║██████╔╝█████╗  ██║     ██║   ██║██║  ██║█████╗
  ██╔═══╝ ██╔══██║██╔══██╗██╔══╝  ██║     ██║   ██║██║  ██║██╔══╝
  ██║     ██║  ██║██║  ██║███████╗╚██████╗╚██████╔╝██████╔╝███████╗
  ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝ ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝
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

// ── Main draw entry point ─────────────────────────────────────────────────────

pub fn draw(f: &mut Frame, state: &AppState) {
    let area = f.area();

    // Horizontal split when sidebar is visible
    let main_area = if state.sidebar_visible {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(0)])
            .split(area);
        super::sidebar::draw_sidebar(f, state, cols[0]);
        cols[1]
    } else {
        area
    };

    let has_chips = !state.attached_files.is_empty();
    let constraints = if has_chips {
        vec![
            Constraint::Length(1),  // tab bar
            Constraint::Min(0),     // content area
            Constraint::Length(1),  // status bar
            Constraint::Length(1),  // stats bar
            Constraint::Length(1),  // attached files chips row
            Constraint::Length(3),  // input box
        ]
    } else {
        vec![
            Constraint::Length(1),  // tab bar
            Constraint::Min(0),     // content area
            Constraint::Length(1),  // status bar
            Constraint::Length(1),  // stats bar
            Constraint::Length(3),  // input box
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(main_area);

    draw_tab_bar(f, state, chunks[0]);

    // Content area — dispatch by active tab
    match state.active_tab {
        Tab::Chat   => super::chat::draw_history(f, state, chunks[1]),
        Tab::Config => super::config_view::draw(f, state, chunks[1]),
        Tab::Stats  => super::stats_view::draw(f, state, chunks[1]),
        Tab::Plan   => super::plan_view::draw(f, state, chunks[1]),
        Tab::Git    => super::git_view::draw(f, state, chunks[1]),
    }

    draw_status_bar(f, state, chunks[2]);
    draw_stats_bar(f, state, chunks[3]);
    if has_chips {
        super::chat::draw_chips(f, state, chunks[4]);
        draw_input(f, state, chunks[5]);
    } else {
        draw_input(f, state, chunks[4]);
    }

    if state.mode == Mode::Palette {
        super::overlays::draw_palette(f, state, area);
    }
    if state.mode == Mode::SlashComplete {
        super::overlays::draw_slash_complete(f, state, area);
    }
    if state.mode == Mode::FilePicker {
        if let Some(fp) = &state.file_picker {
            super::overlays::draw_file_picker(f, fp, area);
        }
    }
    if state.mode == Mode::SessionBrowser {
        if let Some(browser) = &state.session_browser {
            super::overlays::draw_session_browser(f, browser, area);
        }
    }
    if state.mode == Mode::ProfilePicker {
        super::overlays::draw_profile_picker(f, state, area);
    }
    // Plan review is now inline in history (PlanCard entry) — no overlay needed

    if state.diff_overlay_visible {
        super::overlays::draw_diff_overlay(f, state, area);
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn draw_tab_bar(f: &mut Frame, state: &AppState, area: Rect) {
    let tabs: &[(&str, Tab, &str)] = &[
        ("[1] Chat  ", Tab::Chat,   "1"),
        ("[2] Config", Tab::Config, "2"),
        ("[3] Stats ", Tab::Stats,  "3"),
        ("[4] Plan  ", Tab::Plan,   "4"),
        ("[5] Git   ", Tab::Git,    "5"),
    ];

    let mut spans = vec![Span::raw(" ")];
    for (label, tab, _key) in tabs {
        // Hide Plan tab unless a plan has been generated
        if *tab == Tab::Plan && !state.plan_ever_active {
            continue;
        }
        // Hide Git tab when not in a git repo
        if *tab == Tab::Git && !state.git_available {
            continue;
        }
        let active = state.active_tab == *tab;
        let style = if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Rgb(60, 55, 90))
        };
        spans.push(Span::styled(label.to_string(), style));
        spans.push(Span::styled("  ", Style::default()));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Rgb(6, 6, 12))),
        area,
    );
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
        use crate::plan::PlanStatus;
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
    let (status_glyph, status_color) = if state.mode == Mode::AskingUser {
        ("?", Color::Yellow)
    } else if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
        let g = SPINNER_GLYPHS[(state.spinner_tick as usize) % SPINNER_GLYPHS.len()];
        (g, Color::Cyan)
    } else {
        ("▲", Color::White)
    };

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(status_glyph, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled(" parecode", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
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
            "  Ctrl+B sidebar  Ctrl+H history",
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

    // In-flight token indicator (shown while agent is running)
    let inflight_str = if s.inflight_input_tokens > 0 || s.inflight_output_tokens > 0 {
        let total_inf = s.inflight_input_tokens + s.inflight_output_tokens;
        let tools_part = if s.inflight_tool_calls > 0 {
            format!("  {}tools", s.inflight_tool_calls)
        } else {
            String::new()
        };
        format!("  ▶ {}tok (i:{} o:{}){}",
            fmt_k(total_inf), fmt_k(s.inflight_input_tokens), fmt_k(s.inflight_output_tokens),
            tools_part)
    } else {
        String::new()
    };

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
        Span::styled(
            inflight_str,
            Style::default().fg(Color::Cyan),
        ),
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

// ── Input box ─────────────────────────────────────────────────────────────────

fn draw_input(f: &mut Frame, state: &AppState, area: Rect) {
    let (border_color, prompt_color, prompt_char) = match state.mode {
        Mode::AgentRunning   => (Color::Rgb(40, 40, 60),  Color::DarkGray,           "·"),
        Mode::AskingUser     => (Color::Yellow,            Color::Yellow,             "?"),
        Mode::Palette        => (Color::Cyan,              Color::Cyan,               "⌘"),
        Mode::FilePicker     => (Color::Green,             Color::Green,              "@"),
        Mode::SlashComplete  => (Color::Cyan,              Color::Cyan,               "/"),
        Mode::SessionBrowser => (Color::Rgb(110, 90, 200), Color::Rgb(110, 90, 200), "◈"),
        Mode::PlanReview     => (Color::Rgb(200, 140, 0),  Color::Rgb(220, 160, 0),  "◇"),
        Mode::PlanRunning    => (Color::Rgb(40, 40, 60),   Color::DarkGray,          "▶"),
        Mode::UndoPicker     => (Color::Rgb(200, 80, 40),  Color::Rgb(220, 100, 60), "⚠"),
        Mode::ProfilePicker  => (Color::Cyan,              Color::Cyan,               "▸"),
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
        } else if state.mode == Mode::AskingUser {
            Span::styled(
                "type your answer · Enter to send · Ctrl+C to skip",
                Style::default().fg(Color::Rgb(180, 140, 40)),
            )
        } else {
            Span::styled(
                "message · @ attach · Ctrl+B sidebar · Ctrl+P commands",
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
    if matches!(state.mode, Mode::Normal | Mode::AskingUser | Mode::Palette | Mode::FilePicker | Mode::SlashComplete | Mode::PlanReview | Mode::ProfilePicker) {
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
