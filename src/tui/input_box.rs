/// Stylish multiline input box backed by tui-textarea.
///
/// Wraps `TextArea<'static>` and provides:
/// - undo/redo (Ctrl+Z / Ctrl+Y)
/// - text selection (Shift+arrows)
/// - scrolling when content exceeds box height
/// - cursor line highlighting
/// - mode-specific border colours and prompt glyphs
/// - placeholder text when empty
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders},
};
use tui_textarea::{CursorMove, TextArea};

use super::Mode;

// ── InputAction ───────────────────────────────────────────────────────────────

/// What the input box wants the caller to do after handling a key.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    /// User pressed Enter on a non-empty single-line input — caller should submit.
    Submit,
    /// Key was handled by the input box — caller should redraw.
    Handled,
    /// Key was not consumed — caller should handle it (e.g. Esc, Ctrl+P).
    Passthrough,
}

// ── InputBox ──────────────────────────────────────────────────────────────────

pub struct InputBox {
    pub textarea: TextArea<'static>,
}

impl InputBox {
    pub fn new() -> Self {
        let mut ta = TextArea::default();
        ta.set_cursor_line_style(Style::default().bg(Color::Rgb(18, 18, 30)));
        ta.set_cursor_style(
            Style::default()
                .fg(Color::Rgb(80, 160, 255))
                .bg(Color::Rgb(40, 60, 120))
                .add_modifier(Modifier::BOLD),
        );
        ta.set_selection_style(Style::default().bg(Color::Rgb(40, 60, 100)).fg(Color::White));
        // No block — outer block is rendered separately
        ta.set_block(Block::default());
        Self { textarea: ta }
    }

    // ── Text access ───────────────────────────────────────────────────────────

    /// Get the full text as a single String with newlines.
    pub fn get_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Replace all content with the given string.
    pub fn set_text(&mut self, s: &str) {
        // Rebuild the TextArea from scratch
        *self = Self::new();
        if !s.is_empty() {
            for (i, line) in s.split('\n').enumerate() {
                if i > 0 {
                    self.textarea.input(tui_textarea::Input {
                        key: tui_textarea::Key::Enter,
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                }
                for c in line.chars() {
                    self.textarea.input(tui_textarea::Input {
                        key: tui_textarea::Key::Char(c),
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                }
            }
        }
    }

    /// Append text at the end of the current content.
    pub fn insert_str(&mut self, s: &str) {
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::End);
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                self.textarea.input(tui_textarea::Input {
                    key: tui_textarea::Key::Enter,
                    ctrl: false,
                    alt: false,
                    shift: false,
                });
            }
            for c in line.chars() {
                self.textarea.input(tui_textarea::Input {
                    key: tui_textarea::Key::Char(c),
                    ctrl: false,
                    alt: false,
                    shift: false,
                });
            }
        }
    }

    /// Clear all content.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Move the cursor to the very end of all content.
    pub fn move_to_end(&mut self) {
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::End);
    }

    /// Insert a literal newline at the current cursor position.
    pub fn insert_newline(&mut self) {
        self.textarea.input(tui_textarea::Input {
            key: tui_textarea::Key::Enter,
            ctrl: false,
            alt: false,
            shift: false,
        });
    }

    /// True if the text is empty.
    pub fn is_empty(&self) -> bool {
        let lines = self.textarea.lines();
        lines.is_empty() || (lines.len() == 1 && lines[0].is_empty())
    }

    /// Number of lines currently in the textarea.
    pub fn line_count(&self) -> usize {
        self.textarea.lines().len().max(1)
    }

    // ── Key handling ──────────────────────────────────────────────────────────

    /// Handle a key event. Returns what the caller should do next.
    ///
    /// Keys that are NOT consumed (→ Passthrough):
    ///   Esc, Ctrl+C, Ctrl+D, Ctrl+P, Ctrl+H, Ctrl+B, Tab (no files), digit shortcuts
    ///
    /// Keys that are intercepted BEFORE tui-textarea sees them:
    ///   Enter (submit on single-line), Ctrl+Shift+Enter / Shift+Enter (insert newline)
    ///   Ctrl+U (clear line before cursor — NOT undo), Ctrl+K (clear to end of line)
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let shift = m.contains(KeyModifiers::SHIFT);
        let alt = m.contains(KeyModifiers::ALT);
        let no_mod = m == KeyModifiers::NONE;

        match key.code {
            // ── Passthrough keys — caller handles these ───────────────────────
            KeyCode::Esc => return InputAction::Passthrough,
            KeyCode::Char('c') if ctrl && !shift && !alt => return InputAction::Passthrough,
            KeyCode::Char('d') if ctrl && !shift && !alt => return InputAction::Passthrough,
            KeyCode::Char('p') if ctrl && !shift && !alt => return InputAction::Passthrough,
            KeyCode::Char('h') if ctrl && !shift && !alt => return InputAction::Passthrough,
            KeyCode::Char('b') if ctrl && !shift && !alt => return InputAction::Passthrough,

            // ── Shift+Enter / Ctrl+Enter / Alt+Enter — insert newline ────────
            KeyCode::Enter if m.intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                self.textarea.input(tui_textarea::Input {
                    key: tui_textarea::Key::Enter,
                    ctrl: false,
                    alt: false,
                    shift: false,
                });
                return InputAction::Handled;
            }

            // ── Plain Enter — submit (or newline if multi-line context) ───────
            KeyCode::Enter if no_mod => {
                if !self.is_empty() {
                    return InputAction::Submit;
                }
                return InputAction::Handled;
            }

            // ── Ctrl+U — clear from start of line to cursor ───────────────────
            // tui-textarea maps Ctrl+U to Undo — we intercept it
            KeyCode::Char('u') if ctrl && !shift && !alt => {
                // Move to the beginning of the line, select to current cursor, delete
                let (row, col) = self.textarea.cursor();
                if col > 0 {
                    self.textarea.move_cursor(CursorMove::Head);
                    // Select from head to original col
                    for _ in 0..col {
                        self.textarea.input(tui_textarea::Input {
                            key: tui_textarea::Key::Right,
                            ctrl: false,
                            alt: false,
                            shift: true,
                        });
                    }
                    self.textarea.delete_str(col);
                } else if row > 0 {
                    // At col 0 — join with previous line (delete the newline before)
                    self.textarea.move_cursor(CursorMove::Up);
                    self.textarea.move_cursor(CursorMove::End);
                    self.textarea.input(tui_textarea::Input {
                        key: tui_textarea::Key::Delete,
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                }
                return InputAction::Handled;
            }

            // ── Ctrl+K — delete from cursor to end of line ────────────────────
            KeyCode::Char('k') if ctrl && !shift && !alt => {
                let (_, col) = self.textarea.cursor();
                let current_line_len = self.textarea.lines()
                    .get(self.textarea.cursor().0)
                    .map(|l| l.len())
                    .unwrap_or(0);
                let chars_to_end = current_line_len.saturating_sub(col);
                if chars_to_end > 0 {
                    self.textarea.delete_str(chars_to_end);
                }
                return InputAction::Handled;
            }

            // ── Ctrl+Z — undo ─────────────────────────────────────────────────
            KeyCode::Char('z') if ctrl && !shift && !alt => {
                self.textarea.undo();
                return InputAction::Handled;
            }

            // ── Ctrl+Y — redo ─────────────────────────────────────────────────
            KeyCode::Char('y') if ctrl && !shift && !alt => {
                self.textarea.redo();
                return InputAction::Handled;
            }

            KeyCode::Char('r') if ctrl && !shift && !alt => {
                self.clear();
                return InputAction::Handled;
            }

            _ => {}
        }

        // ── Delegate everything else to tui-textarea ──────────────────────────
        let input = tui_textarea::Input::from(key);
        self.textarea.input(input);
        InputAction::Handled
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    /// Draw the input box into `area`.
    /// The caller provides the outer frame; this function renders the border,
    /// placeholder, and the textarea widget.
    /// `display_override` — if Some, show this text instead of the textarea content.
    /// Used by FilePicker / SlashComplete modes so the user sees the query they're typing.
    pub fn draw(&mut self, f: &mut Frame, area: Rect, mode: &Mode, spinner_tick: u32, display_override: Option<&str>) {
        let (border_color, prompt_color, prompt_char, mode_label) = match mode {
            Mode::AgentRunning   => (Color::Rgb(35, 35, 55),  Color::DarkGray,           "·",  ""),
            Mode::AskingUser     => (Color::Rgb(180, 140, 0), Color::Yellow,             "?",  " answer "),
            Mode::Palette        => (Color::Rgb(0, 180, 200), Color::Cyan,               "⌘",  " command "),
            Mode::FilePicker     => (Color::Rgb(0, 180, 100), Color::Green,              "#",  " pick file "),
            Mode::SlashComplete  => (Color::Rgb(0, 180, 200), Color::Cyan,               "/",  " command "),
            Mode::SessionBrowser => (Color::Rgb(110, 90, 200),Color::Rgb(130, 110, 220),"◈",  " sessions "),
            Mode::PlanReview     => (Color::Rgb(200, 140, 0), Color::Rgb(220, 160, 0),  "◇",  " plan review "),
            Mode::PlanRunning    => (Color::Rgb(35, 35, 55),  Color::DarkGray,          "▶",  ""),
            Mode::UndoPicker     => (Color::Rgb(200, 80, 40), Color::Rgb(220, 100, 60), "⚠",  " undo "),
            Mode::ProfilePicker  => (Color::Rgb(0, 180, 200), Color::Cyan,               "▸",  " profile "),
            Mode::HookWizard     => (Color::Rgb(40, 100, 55), Color::Rgb(60, 140, 80),   "⚙",  " hook setup "),
            Mode::Normal         => (Color::Rgb(55, 55, 85),  Color::Rgb(80, 160, 255),  "❯",  ""),
        };

        let box_bg = Color::Rgb(10, 10, 18);

        // Apply mode-specific styling to the textarea
        let cursor_line_bg = match mode {
            Mode::Normal | Mode::AskingUser | Mode::SlashComplete => Color::Rgb(18, 18, 30),
            Mode::FilePicker     => Color::Rgb(10, 22, 16),
            Mode::AgentRunning | Mode::PlanRunning => Color::Rgb(10, 10, 18),
            _                    => Color::Rgb(18, 18, 30),
        };
        let cursor_fg = match mode {
            Mode::AskingUser     => Color::Yellow,
            Mode::FilePicker     => Color::Green,
            Mode::AgentRunning | Mode::PlanRunning => Color::DarkGray,
            _                    => Color::Rgb(80, 160, 255),
        };

        self.textarea.set_cursor_line_style(Style::default().bg(cursor_line_bg));
        self.textarea.set_cursor_style(
            Style::default()
                .fg(cursor_fg)
                .bg(Color::Rgb(30, 50, 90))
                .add_modifier(Modifier::BOLD),
        );
        self.textarea.set_style(Style::default().fg(Color::White).bg(box_bg));

        // Build outer block
        let title_span = if !mode_label.is_empty() {
            Span::styled(mode_label, Style::default().fg(border_color).add_modifier(Modifier::BOLD))
        } else {
            Span::raw("")
        };
        let outer_block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(border_color))
            .title(title_span)
            .style(Style::default().bg(box_bg));

        let inner_area = outer_block.inner(area);
        f.render_widget(outer_block, area);

        if matches!(mode, Mode::AgentRunning | Mode::PlanRunning) {
            // Show animated cancel hint instead of textarea
            let tick = spinner_tick as usize;
            let hints = ["Esc to cancel", "Esc to interrupt", "Esc to stop"];
            let hint = hints[(tick / 20) % hints.len()];
            let line = ratatui::text::Text::from(Line::from(vec![
                Span::styled(
                    format!(" {prompt_char} "),
                    Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(hint, Style::default().fg(Color::Rgb(60, 60, 80))),
            ]));
            f.render_widget(
                ratatui::widgets::Paragraph::new(line).style(Style::default().bg(box_bg)),
                inner_area,
            );
            return;
        }

        if matches!(mode, Mode::PlanReview) {
            // Plan review hint line — no textarea
            let hint = Span::styled(
                "↑↓ navigate  a approve step  e annotate  Enter run when all approved  Esc cancel",
                Style::default().fg(Color::Rgb(100, 80, 30)),
            );
            let line = ratatui::text::Text::from(Line::from(vec![
                Span::styled(
                    format!(" {prompt_char} "),
                    Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
                ),
                hint,
            ]));
            f.render_widget(
                ratatui::widgets::Paragraph::new(line).style(Style::default().bg(box_bg)),
                inner_area,
            );
            return;
        }

        // If a display override is provided (FilePicker / SlashComplete query), show it
        if let Some(display) = display_override {
            let prompt = Span::styled(
                format!(" {prompt_char} "),
                Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
            );
            let content = Span::styled(display.to_string(), Style::default().fg(Color::White));
            // cursor indicator
            let cursor = Span::styled("█", Style::default().fg(prompt_color));
            let text = ratatui::text::Text::from(Line::from(vec![prompt, content, cursor]));
            f.render_widget(
                ratatui::widgets::Paragraph::new(text).style(Style::default().bg(box_bg)),
                inner_area,
            );
            return;
        }

        // Show placeholder when empty (Normal mode only)
        let show_placeholder = self.is_empty()
            && matches!(mode, Mode::Normal | Mode::AskingUser | Mode::Palette | Mode::SlashComplete);

        if show_placeholder {
            let hint_text = match mode {
                Mode::Palette => "search commands…",
                Mode::AskingUser => "type your answer · Enter to send · Esc to skip",
                _ => "message · Alt+Enter newline · # attach file · Ctrl+B sidebar · Ctrl+P commands",
            };
            let prompt = Span::styled(
                format!(" {prompt_char} "),
                Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
            );
            let hint = Span::styled(hint_text, Style::default().fg(Color::Rgb(70, 70, 90)));
            let text = ratatui::text::Text::from(Line::from(vec![prompt, hint]));
            f.render_widget(
                ratatui::widgets::Paragraph::new(text).style(Style::default().bg(box_bg)),
                inner_area,
            );

            // Show example prompts beneath hint in Normal mode with enough height
            if mode == &Mode::Normal && inner_area.height >= 3 {
                let examples = vec![
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("Try: ", Style::default().fg(Color::Rgb(55, 55, 75))),
                        Span::styled("explain this function", Style::default().fg(Color::Rgb(65, 65, 85))),
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("Try: ", Style::default().fg(Color::Rgb(55, 55, 75))),
                        Span::styled("refactor the auth module", Style::default().fg(Color::Rgb(65, 65, 85))),
                    ]),
                ];
                let hints_area = Rect {
                    y: inner_area.y + 1,
                    x: inner_area.x,
                    width: inner_area.width,
                    height: inner_area.height.saturating_sub(1),
                };
                f.render_widget(
                    ratatui::widgets::Paragraph::new(examples).style(Style::default().bg(box_bg)),
                    hints_area,
                );
            }
            return;
        }

        // Render the textarea
        // We need a sub-area that leaves 1 col on the left for the prompt glyph
        // so text appears as " ❯ content" aligned with where the prompt is
        let prompt_width: u16 = 3; // " ❯ " or " # " etc.
        let prompt_area = Rect {
            x: inner_area.x,
            y: inner_area.y,
            width: prompt_width.min(inner_area.width),
            height: 1,
        };
        let text_area = Rect {
            x: inner_area.x + prompt_width.min(inner_area.width),
            y: inner_area.y,
            width: inner_area.width.saturating_sub(prompt_width),
            height: inner_area.height,
        };

        // Draw prompt glyph on first line
        let prompt_line = Line::from(Span::styled(
            format!(" {prompt_char} "),
            Style::default().fg(prompt_color).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(
            ratatui::widgets::Paragraph::new(ratatui::text::Text::from(prompt_line))
                .style(Style::default().bg(box_bg)),
            prompt_area,
        );

        // Remove any block on the textarea itself (outer block already drawn)
        self.textarea.set_block(Block::default());
        f.render_widget(&self.textarea, text_area);
    }
}
