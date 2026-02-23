/// Ratatui-based TUI for PareCode.
///
/// Architecture:
///   main thread:  event loop — crossterm keyboard events + mpsc UiEvent drain
///   agent task:   tokio::spawn — sends UiEvents to main thread via UnboundedSender
///
/// Layout:
///   ┌────────────────────────────────────────────────┐
///   │  conversation history (scrollable, Min(0))     │
///   ├────────────────────────────────────────────────┤
///   │  status bar (1 line)                           │
///   ├────────────────────────────────────────────────┤
///   │  input box (3 lines, fixed)                    │
///   └────────────────────────────────────────────────┘
pub mod render;
pub mod chat;
pub mod overlays;
pub mod config_view;
pub mod stats_view;
pub mod plan_view;
pub mod sidebar;
pub mod git_view;

use std::io;

use anyhow::Result;
use crossterm::{
    event::{
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use std::sync::Arc;

use crate::agent::AgentConfig;
use crate::client::Client;
use crate::config::{ConfigFile, ResolvedConfig};
use crate::mcp::McpClient;
use crate::plan::{self, Plan, StepStatus};
use crate::sessions::{self, ConversationTurn, Session};
use crate::telemetry::{self, SessionStats};

// ── UiEvent — typed events from agent → TUI ──────────────────────────────────

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A streamed text chunk from the model (visible response)
    Chunk(String),
    /// A streamed thinking/reasoning chunk (inside <think>...</think>)
    ThinkingChunk(String),
    /// A tool call is about to execute
    ToolCall { name: String, args_summary: String },
    /// Result of a tool call
    ToolResult { summary: String },
    /// Cache hit on a file read
    CacheHit { path: String },
    /// Loop detected on a tool
    LoopWarning { tool_name: String },
    /// Context was compressed
    BudgetWarning,
    /// Tool call limit reached
    ToolBudgetHit { limit: usize },
    /// Agent finished the task
    AgentDone {
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: usize,
        compressed_count: usize,
        duration_secs: u32,
        cwd: String,
    },
    /// Agent hit an error
    AgentError(String),
    /// Verbose token stats
    TokenStats {
        input: u32,
        output: u32,
        total_input: u32,
        total_output: u32,
    },
    /// Context bar update (called before model API call)
    ContextUpdate { used: usize, total: u32, compressed: bool },

    // ── Plan lifecycle events ─────────────────────────────────────────────────
    /// Plan generation succeeded — enter review mode
    PlanReady(crate::plan::Plan),
    /// Plan generation failed
    PlanGenerateFailed(String),
    /// A plan step is starting
    PlanStepStart { index: usize, total: usize, desc: String },
    /// A plan step completed (pass or fail)
    PlanStepDone { index: usize, passed: bool, error: Option<String> },
    /// All plan steps completed successfully
    PlanComplete { total: usize },
    /// Plan execution stopped at a failed step
    PlanFailed { step: usize, error: String },
    /// A hook ran (on_edit, on_task_done, on_plan_step_done, on_session_start, on_session_end)
    HookOutput { event: String, output: String, exit_code: i32 },
    /// Files changed since the last git checkpoint — drives the Git tab and chat nudge
    GitChanges { stat: String, checkpoint_hash: Option<String>, files_changed: usize },
    /// An auto-commit was created successfully
    GitAutoCommit { message: String },
    /// A git operation failed (non-fatal, display only)
    GitError(String),
}

// ── ConversationEntry — displayable items in history ─────────────────────────

#[derive(Debug, Clone)]
pub enum ConversationEntry {
    UserMessage(String),
    AssistantChunk(String),    // accumulated streaming text (final response)
    ThinkingChunk(String),     // model reasoning inside <think>...</think>
    ToolCall { name: String, args_summary: String },
    ToolResult(String),
    CacheHit(String),
    SystemMsg(String),         // warnings, budget notices, etc.
    HookOutput { event: String, output: String, success: bool },
    TaskComplete {
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: usize,
        compressed_count: usize,
    },
    /// Inline plan card — rendered in history, navigable in PlanReview mode.
    /// The actual step data is read from `AppState::plan_review` at render time.
    PlanCard,
    /// Lightweight nudge shown in chat after a task that changed files.
    /// Directs user to the Git tab (press 5) for the full diff.
    GitNotification { files_changed: usize, _checkpoint_hash: Option<String> },
}

// ── Mode — TUI modal state ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    AgentRunning,
    Palette,        // Ctrl+P command palette
    FilePicker,     // @ file picker
    SlashComplete,  // / inline command autocomplete
    SessionBrowser, // Ctrl+H session history browser
    PlanReview,     // Plan review/annotation overlay
    PlanRunning,    // Plan step currently executing
    UndoPicker,     // Interactive checkpoint picker in Git tab (↑↓ select, Enter confirm, Esc cancel)
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

pub struct SidebarEntry {
    pub id: String,
    pub path: std::path::PathBuf,
    pub project: String,   // cwd basename, max 16 chars
    pub turn_count: usize,
    pub preview: String,   // first message preview, max 26 chars
    pub timestamp: String, // formatted date/time, e.g. "Feb 23 14:05"
    pub is_current: bool,
}

// ── Tab ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Chat,
    Config,
    Stats,
    Plan,
    Git,
}

// ── FilePicker state ──────────────────────────────────────────────────────────

pub struct FilePickerState {
    /// All candidate paths (relative to cwd), gathered once on open
    pub all_files: Vec<String>,
    /// Current filter query (text after the `@`)
    pub query: String,
    /// Index of highlighted item in filtered list
    pub selected: usize,
    /// Byte offset of the `@` character in `AppState::input`
    pub at_offset: usize,
}

impl FilePickerState {
    pub fn filtered(&self) -> Vec<&String> {
        if self.query.is_empty() {
            self.all_files.iter().collect()
        } else {
            let q = self.query.to_lowercase();
            self.all_files
                .iter()
                .filter(|p| p.to_lowercase().contains(&q))
                .collect()
        }
    }
}

/// Collect files under cwd up to depth 5, skipping hidden dirs and common noise.
pub fn gather_files() -> Vec<String> {
    let mut out = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    walk_dir(&cwd, &cwd, 0, 5, &mut out);
    out.sort();
    out
}

fn walk_dir(
    base: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden, target dir, node_modules, __pycache__
        if name_str.starts_with('.')
            || name_str == "target"
            || name_str == "node_modules"
            || name_str == "__pycache__"
        {
            continue;
        }

        if path.is_dir() {
            walk_dir(base, &path, depth + 1, max_depth, out);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.display().to_string());
            }
        }
    }
}

// ── Session browser state ─────────────────────────────────────────────────────

pub struct SessionBrowserState {
    /// (session_id, path, turn_count, first_message_preview)
    pub entries: Vec<(String, std::path::PathBuf, usize, String)>,
    pub selected: usize,
}

impl SessionBrowserState {
    pub fn load() -> Self {
        let entries = sessions::list_sessions()
            .unwrap_or_default()
            .into_iter()
            .take(20)
            .map(|(id, path)| {
                let turns = sessions::load_session_turns(&path).unwrap_or_default();
                let count = turns.len();
                let preview = turns
                    .first()
                    .map(|t| {
                        let s = t.user_message.trim();
                        if s.len() > 60 { format!("{}…", &s[..60]) } else { s.to_string() }
                    })
                    .unwrap_or_else(|| "(empty)".to_string());
                (id, path, count, preview)
            })
            .collect();
        Self { entries, selected: 0 }
    }
}

// ── Plan review state ─────────────────────────────────────────────────────────

pub struct PlanReviewState {
    /// The plan being reviewed
    pub plan: Plan,
    /// Which step is currently selected (cursor)
    pub selected: usize,
    /// True when the user has pressed `e` and is typing an annotation
    pub annotating: bool,
    /// Buffer for annotation input
    pub annotation_input: String,
}

impl PlanReviewState {
    pub fn new(plan: Plan) -> Self {
        Self {
            plan,
            selected: 0,
            annotating: false,
            annotation_input: String::new(),
        }
    }
}

// ── Attached file ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AttachedFile {
    /// Display path (relative, as typed)
    pub path: String,
    /// File contents read at attach time
    pub content: String,
}

// ── AppState ──────────────────────────────────────────────────────────────────

pub struct AppState {
    pub entries: Vec<ConversationEntry>,
    pub input: String,
    pub cursor: usize,        // byte offset in input
    pub mode: Mode,
    pub scroll: usize,        // lines scrolled up in history
    pub profile: String,
    pub model: String,
    pub context_tokens: u32,
    pub ctx_used: usize,
    pub ctx_compressed: bool,
    pub show_timestamps: bool,
    pub palette_query: String,
    pub file_picker: Option<FilePickerState>,
    /// Selected index in the slash-complete dropdown
    pub slash_complete_selected: usize,
    pub cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Files pinned via @, injected into every agent call in this conversation
    pub attached_files: Vec<AttachedFile>,
    /// Which chip is focused (for Del/Backspace removal); None = input focused
    pub focused_chip: Option<usize>,
    /// Incremented every 120ms during AgentRunning for spinner animation
    pub spinner_tick: u32,

    // ── Phase 3: session memory ───────────────────────────────────────────────
    /// Completed turns in this session (persisted + used for context injection)
    pub conversation_turns: Vec<ConversationTurn>,
    /// Current session (file handle + rollback pointer)
    pub session: Option<Session>,
    /// Accumulates assistant text chunks during an agent run (for turn record)
    pub collecting_response: String,
    /// Accumulates tool names during an agent run (for turn record)
    pub collecting_tools: Vec<String>,
    /// True if we loaded a prior session's turns on startup or via /resume
    pub session_resumed: bool,
    /// Session browser overlay state (Some when Mode::SessionBrowser)
    pub session_browser: Option<SessionBrowserState>,
    /// Plan review state (Some when Mode::PlanReview or Mode::PlanRunning)
    pub plan_review: Option<PlanReviewState>,
    /// Step index currently executing (used during PlanRunning)
    pub plan_running_step: usize,
    /// MCP client — shared across all agent runs
    pub mcp: Arc<McpClient>,
    /// Live session telemetry — accumulated across all agent runs
    pub stats: SessionStats,
    /// First line of the current/most-recent task message (for telemetry records)
    pub current_task_preview: String,
    /// Optional cost per 1M input tokens (from profile config) for plan estimates
    pub cost_per_mtok_input: Option<f64>,
    /// Last streamed text snippet (for live status bar display while model is thinking)
    pub last_stream_text: String,
    /// True if the model is currently in a <think> block
    pub stream_in_think: bool,
    /// When false, hooks are suppressed for this session (/hooks off)
    pub hooks_enabled: bool,
    /// Sidebar visible (collapsible session list on left)
    pub sidebar_visible: bool,
    /// True = arrow keys navigate the sidebar instead of scrolling history
    pub sidebar_focused: bool,
    /// Highlighted row index in the sidebar
    pub sidebar_selected: usize,
    /// Loaded sidebar entries (top 30 sessions)
    pub sidebar_entries: Vec<SidebarEntry>,
    /// Currently active tab
    pub active_tab: Tab,
    /// True once any plan has been generated (shows Plan tab)
    pub plan_ever_active: bool,
    /// Scroll offset for the Stats tab
    pub stats_scroll: usize,
    /// Historical telemetry loaded from disk at startup (all-time records)
    pub telemetry_history: Vec<crate::telemetry::TaskRecord>,
    /// Endpoint URL (for Config tab display)
    pub endpoint: String,
    /// Hooks config snapshot for Config tab display
    pub hooks_config: crate::hooks::HookConfig,
    /// Whether hooks are disabled at profile level (for Config tab display)
    pub hooks_disabled_profile: bool,

    // ── Git integration ───────────────────────────────────────────────────────
    /// Whether the cwd is inside a git repo (controls Git tab visibility)
    pub git_available: bool,
    /// Commit hash of the last checkpoint created before a task
    pub last_checkpoint_hash: Option<String>,
    /// `git diff --stat` output for display in the Git tab
    pub git_stat_content: String,
    /// Full `git diff` content for the diff overlay
    pub git_diff_content: String,
    /// Scroll offset within the diff overlay
    pub diff_overlay_scroll: usize,
    /// Whether the full-diff overlay is currently open
    pub diff_overlay_visible: bool,
    /// Cached list of parecode checkpoints (for /undo and Git tab)
    pub git_checkpoints: Vec<crate::git::CheckpointInfo>,
    /// Selected index in the UndoPicker list
    pub undo_picker_selected: usize,
}

impl AppState {
    pub fn new(resolved: &ResolvedConfig, show_timestamps: bool, mcp: Arc<McpClient>) -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            mode: Mode::Normal,
            scroll: 0,
            profile: resolved.profile_name.clone(),
            model: resolved.model.clone(),
            context_tokens: resolved.context_tokens,
            ctx_used: 0,
            ctx_compressed: false,
            show_timestamps,
            palette_query: String::new(),
            file_picker: None,
            slash_complete_selected: 0,
            cancel_tx: None,
            attached_files: Vec::new(),
            focused_chip: None,
            spinner_tick: 0,
            conversation_turns: Vec::new(),
            session: None,
            collecting_response: String::new(),
            collecting_tools: Vec::new(),
            session_resumed: false,
            session_browser: None,
            plan_review: None,
            plan_running_step: 0,
            mcp,
            stats: SessionStats::default(),
            current_task_preview: String::new(),
            cost_per_mtok_input: None,
            last_stream_text: String::new(),
            stream_in_think: false,
            hooks_enabled: true,
            active_tab: Tab::Chat,
            plan_ever_active: false,
            stats_scroll: 0,
            telemetry_history: telemetry::load_all(),
            endpoint: resolved.endpoint.clone(),
            hooks_config: resolved.hooks.clone(),
            hooks_disabled_profile: resolved.hooks_disabled,
            sidebar_visible: false, // set to true after terminal size check in event_loop
            sidebar_focused: false,
            sidebar_selected: 0,
            sidebar_entries: Vec::new(),
            git_available: crate::git::is_git_repo(std::path::Path::new(".")),
            last_checkpoint_hash: None,
            git_stat_content: String::new(),
            git_diff_content: String::new(),
            diff_overlay_scroll: 0,
            diff_overlay_visible: false,
            git_checkpoints: Vec::new(),
            undo_picker_selected: 0,
        }
    }

    fn push(&mut self, entry: ConversationEntry) {
        self.entries.push(entry);
        self.scroll = 0; // auto-scroll to bottom on new content
    }

    fn append_chunk(&mut self, chunk: &str) {
        if let Some(ConversationEntry::AssistantChunk(s)) = self.entries.last_mut() {
            s.push_str(chunk);
        } else {
            self.push(ConversationEntry::AssistantChunk(chunk.to_string()));
        }
    }

    fn append_thinking(&mut self, chunk: &str) {
        if let Some(ConversationEntry::ThinkingChunk(s)) = self.entries.last_mut() {
            s.push_str(chunk);
        } else {
            self.push(ConversationEntry::ThinkingChunk(chunk.to_string()));
        }
    }

    fn apply_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Chunk(c) => {
                self.collecting_response.push_str(&c);
                self.append_chunk(&c);
                self.stream_in_think = false;
                // Keep last ~60 chars of streamed text for the live status indicator
                self.last_stream_text.push_str(&c);
                let len = self.last_stream_text.len();
                if len > 80 {
                    let start = self.last_stream_text.floor_char_boundary(len - 60);
                    self.last_stream_text = self.last_stream_text[start..].to_string();
                }
            }
            UiEvent::ThinkingChunk(c) => {
                self.append_thinking(&c);
                self.stream_in_think = true;
                self.last_stream_text.push_str(&c);
                let len = self.last_stream_text.len();
                if len > 80 {
                    let start = self.last_stream_text.floor_char_boundary(len - 60);
                    self.last_stream_text = self.last_stream_text[start..].to_string();
                }
            }
            UiEvent::ToolCall { name, args_summary } => {
                // Build a compact action string for the session turn record:
                // "edit_file(src/foo.rs)" rather than just "edit_file"
                let action = compact_tool_action(&name, &args_summary);
                self.collecting_tools.push(action);
                self.push(ConversationEntry::ToolCall { name, args_summary });
            }
            UiEvent::ToolResult { summary } => {
                self.push(ConversationEntry::ToolResult(summary));
            }
            UiEvent::CacheHit { path } => {
                self.push(ConversationEntry::CacheHit(path));
            }
            UiEvent::LoopWarning { tool_name } => {
                self.push(ConversationEntry::SystemMsg(
                    format!("⚠ loop detected on {tool_name} — injecting cached result"),
                ));
            }
            UiEvent::BudgetWarning => {
                self.stats.record_budget_enforcement();
                self.push(ConversationEntry::SystemMsg("⟳ context compressed".to_string()));
            }
            UiEvent::ToolBudgetHit { limit } => {
                self.push(ConversationEntry::SystemMsg(
                    format!("■ tool call limit ({limit}) reached"),
                ));
            }
            UiEvent::AgentDone { input_tokens, output_tokens, tool_calls, compressed_count, duration_secs, cwd } => {
                // Record telemetry
                let session_id = self.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
                let record = self.stats.record_task(
                    &session_id,
                    &cwd,
                    &self.current_task_preview.clone(),
                    input_tokens,
                    output_tokens,
                    tool_calls,
                    compressed_count,
                    duration_secs,
                    &self.model.clone(),
                    &self.profile.clone(),
                );
                telemetry::append_record(&record);

                self.push(ConversationEntry::TaskComplete {
                    input_tokens,
                    output_tokens,
                    tool_calls,
                    compressed_count,
                });
                self.finalize_turn();
                self.last_stream_text.clear();
                self.stream_in_think = false;
                if self.mode == Mode::PlanRunning {
                    self.mode = Mode::PlanRunning; // step_done_tx will drive transition
                } else {
                    self.mode = Mode::Normal;
                }
                self.cancel_tx = None;
            }
            UiEvent::AgentError(e) => {
                self.push(ConversationEntry::SystemMsg(format!("✗ {e}")));
                self.finalize_turn();
                self.last_stream_text.clear();
                self.stream_in_think = false;
                if self.mode == Mode::PlanRunning {
                    self.mode = Mode::PlanRunning; // step_done_tx will drive transition
                } else {
                    self.mode = Mode::Normal;
                }
                self.cancel_tx = None;
            }
            UiEvent::TokenStats { input, output, total_input, total_output } => {
                self.push(ConversationEntry::SystemMsg(
                    format!("· i:{input} o:{output} ∑i:{total_input} ∑o:{total_output}"),
                ));
            }
            UiEvent::ContextUpdate { used, total, compressed } => {
                self.ctx_used = used;
                self.ctx_compressed = compressed;
                if total > 0 {
                    let pct = (used as f32 / total as f32 * 100.0) as u32;
                    self.stats.update_peak_context(pct);
                }
            }

            // ── Plan lifecycle ────────────────────────────────────────────────
            UiEvent::PlanReady(generated_plan) => {
                self.plan_review = Some(PlanReviewState::new(generated_plan));
                self.plan_ever_active = true;
                // Push the plan card inline into history — no overlay
                self.push(ConversationEntry::PlanCard);
                self.mode = Mode::PlanReview;
            }
            UiEvent::PlanGenerateFailed(e) => {
                self.push(ConversationEntry::SystemMsg(
                    format!("✗ plan generation failed: {e}"),
                ));
                self.mode = Mode::Normal;
            }
            UiEvent::PlanStepStart { index, total, desc } => {
                self.push(ConversationEntry::SystemMsg(
                    format!("▶ step {}/{}: {desc}", index + 1, total),
                ));
                self.plan_running_step = index;
                self.mode = Mode::PlanRunning;
                if let Some(pr) = &mut self.plan_review {
                    pr.plan.steps[index].status = StepStatus::Running;
                    pr.plan.current = index;
                }
                // Reset collectors for the new step
                self.collecting_response.clear();
                self.collecting_tools.clear();
            }
            UiEvent::PlanStepDone { index, passed, error } => {
                if passed {
                    if let Some(pr) = &mut self.plan_review {
                        pr.plan.steps[index].status = StepStatus::Pass;
                    }
                    let total = self.plan_review.as_ref().map(|pr| pr.plan.steps.len()).unwrap_or(0);
                    self.push(ConversationEntry::SystemMsg(
                        format!("  ✓ step {}/{} complete", index + 1, total),
                    ));
                } else {
                    if let Some(pr) = &mut self.plan_review {
                        pr.plan.steps[index].status = StepStatus::Fail;
                    }
                    let total = self.plan_review.as_ref().map(|pr| pr.plan.steps.len()).unwrap_or(0);
                    let msg = error.unwrap_or_else(|| "unknown error".to_string());
                    self.push(ConversationEntry::SystemMsg(
                        format!("  ✗ step {}/{} failed: {msg}", index + 1, total),
                    ));
                }
            }
            UiEvent::PlanComplete { total } => {
                if let Some(pr) = &mut self.plan_review {
                    pr.plan.status = crate::plan::PlanStatus::Complete;
                }
                self.push(ConversationEntry::SystemMsg(
                    format!("✓ plan complete — {total} step{} executed", if total == 1 { "" } else { "s" }),
                ));
                self.mode = Mode::Normal;
            }
            UiEvent::PlanFailed { step, error } => {
                if let Some(pr) = &mut self.plan_review {
                    pr.plan.status = crate::plan::PlanStatus::Failed;
                }
                self.push(ConversationEntry::SystemMsg(
                    format!("  ✗ step {} failed: {error}", step + 1),
                ));
                self.push(ConversationEntry::SystemMsg(
                    "  plan paused — fix the issue and use /plan to continue, or Esc to cancel".to_string(),
                ));
                self.mode = Mode::Normal;
            }
            UiEvent::HookOutput { event, output, exit_code } => {
                let success = exit_code == 0;
                self.push(ConversationEntry::HookOutput { event, output, success });
            }

            // ── Git events ────────────────────────────────────────────────────
            UiEvent::GitChanges { stat, checkpoint_hash, files_changed } => {
                self.last_checkpoint_hash = checkpoint_hash.clone();
                self.git_stat_content = stat;
                // Refresh checkpoint list for the Git tab
                if let Some(repo) = crate::git::GitRepo::open(std::path::Path::new(".")) {
                    self.git_checkpoints = repo.list_checkpoints().unwrap_or_default();
                }
                // Lightweight nudge in chat — one line, directs to Git tab
                self.push(ConversationEntry::GitNotification { files_changed, _checkpoint_hash: checkpoint_hash });
            }
            UiEvent::GitAutoCommit { message } => {
                self.push(ConversationEntry::SystemMsg(format!("✓ committed: {message}")));
            }
            UiEvent::GitError(e) => {
                self.push(ConversationEntry::SystemMsg(format!("⚠ git: {e}")));
            }
        }
    }
}

// ── Turn finalisation ─────────────────────────────────────────────────────────

impl AppState {
    /// Called when an agent run completes (success or error).
    /// Saves the turn to memory and to disk, then resets collectors.
    fn finalize_turn(&mut self) {
        // Find the user message that kicked off this run (last UserMessage entry)
        let user_msg = self
            .entries
            .iter()
            .rev()
            .find_map(|e| {
                if let ConversationEntry::UserMessage(s) = e {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let idx = self.conversation_turns.len();
        let turn = ConversationTurn {
            turn_index: idx,
            timestamp: chrono::Utc::now().timestamp(),
            user_message: user_msg,
            agent_response: std::mem::take(&mut self.collecting_response),
            tool_summary: self.collecting_tools.drain(..).collect::<Vec<_>>().join(", "),
        };

        // Persist before pushing into memory
        if let Some(session) = &self.session {
            let _ = sessions::append_turn(&session.path, &turn);
        }

        self.conversation_turns.push(turn);

        // Advance active_turn pointer to the new turn
        if let Some(session) = &mut self.session {
            session.active_turn = self.conversation_turns.len() - 1;
        }

        // Refresh sidebar so turn count stays current
        if self.sidebar_visible {
            self.sidebar_entries = load_sidebar_entries(&self.session);
        }
    }
}

// ── Palette commands ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PaletteCommand {
    key: &'static str,
    label: &'static str,
}

fn palette_commands() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand { key: "/plan",        label: "Generate and review a plan for a task" },
        PaletteCommand { key: "/quick",       label: "Run a lightweight single-shot query" },
        PaletteCommand { key: "/init",        label: "Generate .parecode/conventions.md for this project" },
        PaletteCommand { key: "/cd",          label: "Change working directory" },
        PaletteCommand { key: "/profile",     label: "Switch profile" },
        PaletteCommand { key: "/profiles",    label: "List profiles" },
        PaletteCommand { key: "/ts",          label: "Toggle timestamps" },
        PaletteCommand { key: "/stats",       label: "Open stats tab  (/stats reset to clear history)" },
        PaletteCommand { key: "/hooks",       label: "Toggle hooks on/off (or /hooks on|off)" },
        PaletteCommand { key: "/list-hooks",  label: "Show all configured hooks and their status" },
        PaletteCommand { key: "/undo",        label: "Revert to last git checkpoint (/undo N for Nth)" },
        PaletteCommand { key: "/diff",        label: "Open full diff overlay for last task changes" },
        PaletteCommand { key: "/clear",       label: "Clear conversation" },
        PaletteCommand { key: "/sessions",    label: "List recent sessions  (or Ctrl+H)" },
        PaletteCommand { key: "/resume",      label: "Resume a previous session" },
        PaletteCommand { key: "/rollback",    label: "Roll back to previous turn" },
        PaletteCommand { key: "/new",          label: "Start a fresh session" },
        PaletteCommand { key: "/help",        label: "Show help" },
        PaletteCommand { key: "/quit",        label: "Quit" },
    ]
}

/// Returns palette commands whose key or label contains the current input query.
fn slash_filtered(input: &str) -> Vec<PaletteCommand> {
    let q = input.to_lowercase();
    palette_commands()
        .into_iter()
        .filter(|c| c.key.contains(q.as_str()) || c.label.to_lowercase().contains(q.as_str()))
        .collect()
}

// ── Terminal setup / teardown ─────────────────────────────────────────────────

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

// ── Main TUI run loop ─────────────────────────────────────────────────────────

pub async fn run(
    file: ConfigFile,
    initial: ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    show_timestamps: bool,
) -> Result<()> {
    let mut terminal = setup_terminal()?;

    // Panic hook — restore terminal before printing panic
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        orig_hook(info);
    }));

    let result = event_loop(
        &mut terminal,
        file,
        initial,
        verbose,
        dry_run,
        show_timestamps,
    )
    .await;

    restore_terminal(&mut terminal);
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut file: ConfigFile,
    mut resolved: ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    show_timestamps: bool,
) -> Result<()> {
    let mcp = McpClient::new(&resolved.mcp_servers).await;

    // ── Hook bootstrap ────────────────────────────────────────────────────────
    // If the active profile has no hooks configured and hooks aren't disabled,
    // detect the project language, write a hooks section into the config file
    // (once, with commented examples for all events), and update resolved so
    // all subsequent calls read from config rather than re-detecting.
    if !resolved.hooks_disabled && resolved.hooks.is_empty() {
        let detected = crate::hooks::write_hooks_to_config(&resolved.profile_name);
        if !detected.is_empty() {
            resolved.hooks = detected;
        }
    }

    let mut state = AppState::new(&resolved, show_timestamps, mcp);
    state.cost_per_mtok_input = resolved.cost_per_mtok_input;

    // Auto-show sidebar when terminal is wide enough
    if let Ok((w, _)) = crossterm::terminal::size() {
        state.sidebar_visible = w >= 110;
    }
    state.sidebar_entries = load_sidebar_entries(&state.session);

    // ── Hook startup summary ──────────────────────────────────────────────────
    if resolved.hooks_disabled {
        state.push(ConversationEntry::SystemMsg("⚙ hooks disabled for this profile".to_string()));
    } else if let Some(summary) = resolved.hooks.summary() {
        state.push(ConversationEntry::SystemMsg(format!("⚙ hooks  {summary}  (/list-hooks for details)")));
    }

    // Auto-resume the most recent session for this cwd (load turns for context + display)
    let cwd = cwd_str();
    if let Some((_id, path)) = sessions::find_latest_for_cwd(&cwd) {
        if let Ok(turns) = sessions::load_session_turns(&path) {
            if !turns.is_empty() {
                // Replay as display entries so history is visible
                for t in &turns {
                    state.entries.push(ConversationEntry::UserMessage(t.user_message.clone()));
                    if !t.agent_response.is_empty() {
                        state.entries.push(ConversationEntry::AssistantChunk(t.agent_response.clone()));
                    }
                }
                let count = turns.len();
                state.conversation_turns = turns;
                state.session_resumed = true;
                state.push(ConversationEntry::SystemMsg(
                    format!("↩ resumed session · {count} turn{} · /new for a fresh start",
                        if count == 1 { "" } else { "s" }),
                ));
            }
        }
    }

    // Open a new session file for this TUI invocation (non-fatal if storage unavailable)
    match sessions::open_session(&cwd) {
        Ok(session) => {
            if let Some(s) = &mut state.session {
                // Update active_turn to match loaded turns
                s.active_turn = state.conversation_turns.len().saturating_sub(1);
            }
            state.session = Some(session);
            // Keep only the 10 most recent non-empty sessions; delete older ones
            sessions::prune_old_sessions(10);
            // Reload sidebar now that state.session is set — this correctly marks is_current
            state.sidebar_entries = load_sidebar_entries(&state.session);
        }
        Err(e) => {
            state.push(ConversationEntry::SystemMsg(
                format!("session: storage unavailable: {e}"),
            ));
        }
    }
    // Set active_turn on the freshly opened session to match loaded turns
    if let Some(s) = &mut state.session {
        s.active_turn = state.conversation_turns.len().saturating_sub(1);
    }

    // Channel: agent → TUI
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();

    // Fire on_session_start hooks (non-blocking — output shown in TUI)
    {
        let session_hooks = resolve_hooks(&resolved, state.hooks_enabled);
        if !session_hooks.on_session_start.is_empty() {
            let tx = ui_tx.clone();
            let cmds = session_hooks.on_session_start.clone();
            tokio::spawn(async move {
                for cmd in &cmds {
                    let hr = crate::hooks::run_hook(cmd).await;
                    let _ = tx.send(UiEvent::HookOutput {
                        event: "on_session_start".to_string(),
                        output: hr.output,
                        exit_code: hr.exit_code,
                    });
                }
            });
        }
    }

    let mut crossterm_events = EventStream::new();
    let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(120));

    // Splash screen
    terminal.draw(|f| render::draw_splash(f))?;
    tokio::time::sleep(tokio::time::Duration::from_millis(1400)).await;
    terminal.draw(|f| render::draw(f, &state))?;

    loop {
        tokio::select! {
            // ── Animation tick ────────────────────────────────────────────────
            _ = ticker.tick() => {
                if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
                    state.spinner_tick = state.spinner_tick.wrapping_add(1);
                    terminal.draw(|f| render::draw(f, &state))?;
                }
            }

            // ── Drain UI events from agent ────────────────────────────────────
            Some(ev) = ui_rx.recv() => {
                state.apply_event(ev);
                terminal.draw(|f| render::draw(f, &state))?;
            }

            // ── Keyboard/resize events ────────────────────────────────────────
            Some(Ok(ev)) = crossterm_events.next() => {
                match ev {
                    Event::Key(key) => {
                        let keep = handle_key(
                            key,
                            &mut state,
                            &mut resolved,
                            &mut file,
                            verbose,
                            dry_run,
                            ui_tx.clone(),
                        )?;
                        if !keep { break; }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                terminal.draw(|f| render::draw(f, &state))?;
            }
        }
    }

    // Fire on_session_end hooks synchronously before returning
    let session_end_hooks = resolve_hooks(&resolved, state.hooks_enabled);
    for cmd in &session_end_hooks.on_session_end {
        let hr = crate::hooks::run_hook(cmd).await;
        // Hooks run after TUI teardown — just print to stderr so they're visible
        if !hr.output.trim().is_empty() {
            eprintln!("⚙ hook (on_session_end): {}", hr.output);
        }
    }

    Ok(())
}

// ── Key handler ───────────────────────────────────────────────────────────────

fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    resolved: &mut ResolvedConfig,
    file: &mut ConfigFile,
    verbose: bool,
    dry_run: bool,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<bool> {
    // ── Sidebar focused navigation ────────────────────────────────────────────
    if state.sidebar_focused && state.sidebar_visible {
        match key.code {
            KeyCode::Up => {
                if state.sidebar_selected > 0 {
                    state.sidebar_selected -= 1;
                }
                return Ok(true);
            }
            KeyCode::Down => {
                if state.sidebar_selected + 1 < state.sidebar_entries.len() {
                    state.sidebar_selected += 1;
                }
                return Ok(true);
            }
            KeyCode::Enter => {
                let idx = state.sidebar_selected;
                if let Some(entry) = state.sidebar_entries.get(idx) {
                    let path = entry.path.clone();
                    let id = entry.id.clone();
                    state.sidebar_focused = false;
                    match sessions::load_session_turns(&path) {
                        Ok(turns) if !turns.is_empty() => {
                            let count = turns.len();
                            state.entries.clear();
                            state.scroll = 0;
                            for t in &turns {
                                state.entries.push(ConversationEntry::UserMessage(t.user_message.clone()));
                                if !t.agent_response.is_empty() {
                                    state.entries.push(ConversationEntry::AssistantChunk(t.agent_response.clone()));
                                }
                            }
                            // Update state.session to the newly selected session so
                            // is_current highlights correctly in the sidebar.
                            let cwd = id.splitn(2, '_').nth(1).unwrap_or("unknown").to_string();
                            state.session = Some(sessions::Session {
                                id: id.clone(),
                                _cwd: cwd,
                                _turns: turns.clone(),
                                active_turn: count.saturating_sub(1),
                                path: path.clone(),
                            });
                            state.conversation_turns = turns;
                            state.session_resumed = true;
                            state.push(ConversationEntry::SystemMsg(
                                format!("✓ resumed {id} ({count} turns)"),
                            ));
                            // Refresh sidebar to mark new current
                            state.sidebar_entries = load_sidebar_entries(&state.session);
                        }
                        Ok(_) => {
                            state.push(ConversationEntry::SystemMsg("session is empty".to_string()));
                        }
                        Err(e) => {
                            state.push(ConversationEntry::SystemMsg(format!("resume error: {e}")));
                        }
                    }
                }
                return Ok(true);
            }
            KeyCode::Esc => {
                state.sidebar_focused = false;
                return Ok(true);
            }
            // Any char typed while sidebar is focused: unfocus and pass through
            KeyCode::Char(_) => {
                state.sidebar_focused = false;
                // fall through to normal char handling below
            }
            _ => {
                return Ok(true);
            }
        }
    }

    // ── Diff overlay key intercept ────────────────────────────────────────────
    // When the full-diff overlay is open, intercept all scrolling and dismiss keys.
    if state.diff_overlay_visible {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                state.diff_overlay_scroll = state.diff_overlay_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.diff_overlay_scroll = state.diff_overlay_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                state.diff_overlay_scroll = state.diff_overlay_scroll.saturating_add(20);
            }
            KeyCode::PageUp => {
                state.diff_overlay_scroll = state.diff_overlay_scroll.saturating_sub(20);
            }
            KeyCode::Char('d') | KeyCode::Esc => {
                state.diff_overlay_visible = false;
            }
            _ => {}
        }
        return Ok(true);
    }

    // ── UndoPicker mode ───────────────────────────────────────────────────────
    // Interactive checkpoint picker: ↑↓ navigate, Enter confirm, Esc cancel.
    if state.mode == Mode::UndoPicker {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if state.undo_picker_selected > 0 {
                    state.undo_picker_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if state.undo_picker_selected + 1 < state.git_checkpoints.len() {
                    state.undo_picker_selected += 1;
                }
            }
            KeyCode::Enter => {
                let idx = state.undo_picker_selected;
                state.mode = Mode::Normal;
                if let Some(cp) = state.git_checkpoints.get(idx).cloned() {
                    match crate::git::GitRepo::open(std::path::Path::new(".")) {
                        Some(repo) => match repo.undo(idx + 1) {
                            Ok(()) => {
                                state.push(ConversationEntry::SystemMsg(format!(
                                    "✓ reverted to checkpoint {}  \"{}\"",
                                    cp.short_hash, cp.message
                                )));
                                // Refresh git state after reset
                                state.git_checkpoints = repo.list_checkpoints().unwrap_or_default();
                                state.last_checkpoint_hash = None;
                                state.git_stat_content = String::new();
                                state.git_diff_content = String::new();
                            }
                            Err(e) => {
                                state.push(ConversationEntry::SystemMsg(
                                    format!("undo failed: {e}"),
                                ));
                            }
                        },
                        None => {
                            state.push(ConversationEntry::SystemMsg(
                                "not in a git repository".to_string(),
                            ));
                        }
                    }
                }
            }
            KeyCode::Esc => {
                state.mode = Mode::Normal;
            }
            _ => {}
        }
        return Ok(true);
    }

    // ── SessionBrowser mode ───────────────────────────────────────────────────
    if state.mode == Mode::SessionBrowser {
        if let Some(browser) = &mut state.session_browser {
            match key.code {
                KeyCode::Esc => {
                    state.mode = Mode::Normal;
                    state.session_browser = None;
                }
                KeyCode::Up => {
                    if browser.selected > 0 {
                        browser.selected -= 1;
                    }
                }
                KeyCode::Down => {
                    if browser.selected + 1 < browser.entries.len() {
                        browser.selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let idx = browser.selected;
                    if let Some((id, path, _, _)) = browser.entries.get(idx).cloned() {
                        state.mode = Mode::Normal;
                        state.session_browser = None;
                        match sessions::load_session_turns(&path) {
                            Ok(turns) if !turns.is_empty() => {
                                let count = turns.len();
                                state.entries.clear();
                                state.scroll = 0;
                                for t in &turns {
                                    state.entries.push(ConversationEntry::UserMessage(t.user_message.clone()));
                                    if !t.agent_response.is_empty() {
                                        state.entries.push(ConversationEntry::AssistantChunk(t.agent_response.clone()));
                                    }
                                }
                                state.conversation_turns = turns;
                                state.session_resumed = true;
                                if let Some(s) = &mut state.session {
                                    s.active_turn = count.saturating_sub(1);
                                }
                                state.push(ConversationEntry::SystemMsg(
                                    format!("✓ resumed {id} ({count} turns)"),
                                ));
                            }
                            Ok(_) => {
                                state.push(ConversationEntry::SystemMsg("session is empty".to_string()));
                            }
                            Err(e) => {
                                state.push(ConversationEntry::SystemMsg(format!("resume error: {e}")));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return Ok(true);
    }

    // ── PlanReview mode ───────────────────────────────────────────────────────
    if state.mode == Mode::PlanReview {
        if let Some(pr) = &mut state.plan_review {
            if pr.annotating {
                // Typing an annotation for the selected step
                match key.code {
                    KeyCode::Esc => {
                        pr.annotating = false;
                        pr.annotation_input.clear();
                    }
                    KeyCode::Enter => {
                        let note = pr.annotation_input.trim().to_string();
                        if !note.is_empty() {
                            pr.plan.steps[pr.selected].user_annotation = Some(note);
                        }
                        pr.annotating = false;
                        pr.annotation_input.clear();
                    }
                    KeyCode::Backspace => { pr.annotation_input.pop(); }
                    KeyCode::Char(c) => { pr.annotation_input.push(c); }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Esc => {
                        state.mode = Mode::Normal;
                        state.plan_review = None;
                        state.push(ConversationEntry::SystemMsg("plan cancelled".to_string()));
                    }
                    KeyCode::Up => {
                        if pr.selected > 0 { pr.selected -= 1; }
                    }
                    KeyCode::Down => {
                        if pr.selected + 1 < pr.plan.steps.len() { pr.selected += 1; }
                    }
                    KeyCode::Char('e') => {
                        // Annotate selected step
                        let existing = pr.plan.steps[pr.selected]
                            .user_annotation.clone().unwrap_or_default();
                        pr.annotation_input = existing;
                        pr.annotating = true;
                    }
                    KeyCode::Char('d') => {
                        // Delete annotation on selected step
                        pr.plan.steps[pr.selected].user_annotation = None;
                    }
                    KeyCode::Char('r') => {
                        state.push(ConversationEntry::SystemMsg(
                            "use /plan \"task\" to regenerate the plan".to_string()
                        ));
                    }
                    KeyCode::Char('a') => {
                        // Approve current step — mark reviewed, advance cursor
                        pr.plan.steps[pr.selected].status = StepStatus::Approved;
                        if pr.selected + 1 < pr.plan.steps.len() {
                            pr.selected += 1;
                        }
                        // No execution yet — user approves all steps first, then Enter runs
                    }
                    KeyCode::Enter => {
                        // Execute only if ALL steps are approved (or already passed)
                        let all_approved = pr.plan.steps.iter().all(|s| {
                            matches!(s.status, StepStatus::Approved | StepStatus::Pass)
                        });
                        if all_approved {
                            let plan = pr.plan.clone();
                            state.mode = Mode::PlanRunning;
                            launch_plan(plan, state, resolved, verbose, dry_run, ui_tx);
                        } else {
                            // Jump cursor to first unreviewed step
                            if let Some(idx) = pr.plan.steps.iter()
                                .position(|s| s.status == StepStatus::Pending)
                            {
                                pr.selected = idx;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        return Ok(true);
    }

    // ── SlashComplete mode ────────────────────────────────────────────────────
    if state.mode == Mode::SlashComplete {
        match key.code {
            KeyCode::Esc => {
                state.mode = Mode::Normal;
            }
            KeyCode::Up => {
                let count = slash_filtered(&state.input).len();
                if count > 0 {
                    state.slash_complete_selected =
                        (state.slash_complete_selected + count - 1) % count;
                }
            }
            KeyCode::Down => {
                let count = slash_filtered(&state.input).len();
                if count > 0 {
                    state.slash_complete_selected =
                        (state.slash_complete_selected + 1) % count;
                }
            }
            KeyCode::Enter | KeyCode::Tab => {
                let matches = slash_filtered(&state.input);
                if let Some(cmd) = matches.get(state.slash_complete_selected) {
                    state.input = cmd.key.to_string();
                    // If the command takes an argument (not /quit, /clear etc.)
                    // add a trailing space so user can type straight away
                    let no_arg = matches!(
                        cmd.key,
                        "/quit" | "/exit" | "/q" | "/clear" | "/sessions"
                        | "/new" | "/help" | "/h" | "/ts" | "/list-hooks"
                        | "/profiles" | "/init" | "/stats"
                    );
                    if !no_arg {
                        state.input.push(' ');
                    }
                    state.cursor = state.input.len();
                }
                state.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                if state.input.len() <= 1 {
                    // Backspaced past `/` — cancel
                    state.input.clear();
                    state.cursor = 0;
                    state.mode = Mode::Normal;
                } else {
                    state.input.pop();
                    state.cursor = state.input.len();
                    state.slash_complete_selected = 0;
                }
            }
            KeyCode::Char(c) => {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                state.input.push_str(s);
                state.cursor = state.input.len();
                state.slash_complete_selected = 0;
                // If input no longer starts with `/`, drop back to normal
                if !state.input.starts_with('/') {
                    state.mode = Mode::Normal;
                }
            }
            _ => {}
        }
        return Ok(true);
    }

    // ── FilePicker mode ───────────────────────────────────────────────────────
    if state.mode == Mode::FilePicker {
        if let Some(fp) = &mut state.file_picker {
            match key.code {
                KeyCode::Esc => {
                    // Cancel: strip the `@` and query from input
                    let at = fp.at_offset;
                    state.input.truncate(at);
                    state.cursor = state.input.len();
                    state.mode = Mode::Normal;
                    state.file_picker = None;
                }
                KeyCode::Up => {
                    if fp.selected > 0 {
                        fp.selected -= 1;
                    }
                }
                KeyCode::Down => {
                    let count = fp.filtered().len();
                    if fp.selected + 1 < count {
                        fp.selected += 1;
                    }
                }
                KeyCode::Enter | KeyCode::Tab => {
                    let filtered = fp.filtered();
                    if let Some(chosen) = filtered.get(fp.selected) {
                        let chosen = chosen.to_string();
                        let at = fp.at_offset;
                        // Remove the @ and query from input (file goes to chips, not text)
                        state.input.truncate(at);
                        state.cursor = state.input.len();
                        // Attach file if not already attached
                        if !state.attached_files.iter().any(|f| f.path == chosen) {
                            match std::fs::read_to_string(&chosen) {
                                Ok(content) => {
                                    state.attached_files.push(AttachedFile { path: chosen, content });
                                }
                                Err(e) => {
                                    state.push(ConversationEntry::SystemMsg(
                                        format!("@ error reading {}: {e}", chosen),
                                    ));
                                }
                            }
                        }
                    }
                    state.mode = Mode::Normal;
                    state.file_picker = None;
                }
                KeyCode::Backspace => {
                    if fp.query.pop().is_none() {
                        // Backspaced past `@` — cancel picker
                        let at = fp.at_offset;
                        state.input.truncate(at);
                        state.cursor = state.input.len();
                        state.mode = Mode::Normal;
                        state.file_picker = None;
                    } else {
                        fp.selected = 0;
                    }
                }
                KeyCode::Char(c) => {
                    fp.query.push(c);
                    fp.selected = 0;
                    // Mirror into input so the user sees what they're typing
                    state.input.push(c);
                    state.cursor = state.input.len();
                }
                _ => {}
            }
        }
        return Ok(true);
    }

    // ── Palette mode ──────────────────────────────────────────────────────────
    if state.mode == Mode::Palette {
        match key.code {
            KeyCode::Esc => {
                state.mode = Mode::Normal;
                state.palette_query.clear();
            }
            KeyCode::Enter => {
                let cmd = state.palette_query.trim().to_string();
                state.mode = Mode::Normal;
                state.palette_query.clear();
                if !cmd.is_empty() {
                    let input = if cmd.starts_with('/') { cmd } else { format!("/{cmd}") };
                    // /plan and /quick need ui_tx — handle them here before execute_command
                    if input.starts_with("/plan ") || input == "/plan" {
                        let task = input.trim_start_matches("/plan").trim().to_string();
                        if task.is_empty() {
                            state.push(ConversationEntry::SystemMsg(
                                "usage: /plan \"describe the task\"".to_string(),
                            ));
                        } else {
                            generate_and_show_plan(task, state, resolved, ui_tx.clone());
                        }
                    } else if input.starts_with("/quick ") || input == "/quick" {
                        let task = input.trim_start_matches("/quick").trim().to_string();
                        if task.is_empty() {
                            state.push(ConversationEntry::SystemMsg(
                                "usage: /quick \"task\"".to_string(),
                            ));
                        } else {
                            launch_quick(task, state, resolved, verbose, dry_run, ui_tx)?;
                        }
                    } else {
                        execute_command(&input, state, resolved, file)?;
                    }
                }
            }
            KeyCode::Backspace => {
                state.palette_query.pop();
            }
            KeyCode::Char(c) => {
                state.palette_query.push(c);
            }
            _ => {}
        }
        return Ok(true);
    }

    // ── Normal / AgentRunning mode ─────────────────────────────────────────
    match (key.modifiers, key.code) {
        // Ctrl+C — cancel agent/plan or quit
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
                if let Some(tx) = state.cancel_tx.take() {
                    let _ = tx.send(());
                }
                // For plan running, return to normal — the spawn will see the channel closed
                if state.mode == Mode::PlanRunning {
                    state.mode = Mode::Normal;
                    state.push(ConversationEntry::SystemMsg("plan cancelled".to_string()));
                }
            } else {
                return Ok(false);
            }
        }
        // Ctrl+D — quit
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            return Ok(false);
        }
        // Ctrl+P — open palette
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
            if state.mode == Mode::Normal {
                state.mode = Mode::Palette;
            }
        }
        // Ctrl+H — open session history browser
        (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
            if state.mode == Mode::Normal {
                state.session_browser = Some(SessionBrowserState::load());
                state.mode = Mode::SessionBrowser;
            }
        }
        // Ctrl+B — toggle sidebar visible (does not change focus)
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
            if state.mode == Mode::Normal || state.sidebar_focused {
                state.sidebar_visible = !state.sidebar_visible;
                if state.sidebar_visible {
                    state.sidebar_entries = load_sidebar_entries(&state.session);
                } else {
                    state.sidebar_focused = false;
                }
            }
        }
        // Tab — when sidebar is visible + input empty: enter sidebar focus
        (KeyModifiers::NONE, KeyCode::Tab)
            if state.sidebar_visible
                && state.input.is_empty()
                && state.mode == Mode::Normal
                && state.attached_files.is_empty() => {
            state.sidebar_focused = true;
        }
        // 1-4 — switch tabs (only when input is empty and not running)
        (KeyModifiers::NONE, KeyCode::Char('1')) if state.input.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Chat;
        }
        (KeyModifiers::NONE, KeyCode::Char('2')) if state.input.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Config;
        }
        (KeyModifiers::NONE, KeyCode::Char('3')) if state.input.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Stats;
        }
        (KeyModifiers::NONE, KeyCode::Char('4')) if state.input.is_empty()
            && state.mode == Mode::Normal
            && state.plan_ever_active => {
            state.active_tab = Tab::Plan;
        }
        (KeyModifiers::NONE, KeyCode::Char('5')) if state.input.is_empty()
            && state.mode == Mode::Normal
            && state.git_available => {
            state.active_tab = Tab::Git;
            // Refresh git tab content on switch
            git_view::load_git_tab(state);
        }
        // 'u' in Git tab opens the checkpoint picker
        (KeyModifiers::NONE, KeyCode::Char('u')) if state.input.is_empty()
            && state.mode == Mode::Normal
            && state.active_tab == Tab::Git => {
            let _ = execute_command("/undo", state, resolved, file)?;
        }
        // 'd' opens the full-diff overlay (only in Normal mode with git available)
        (KeyModifiers::NONE, KeyCode::Char('d')) if state.input.is_empty()
            && state.mode == Mode::Normal
            && state.git_available => {
            if let Some(repo) = crate::git::GitRepo::open(std::path::Path::new(".")) {
                let ref_pt = state.last_checkpoint_hash.as_deref().unwrap_or("HEAD");
                match repo.diff_full_from(ref_pt) {
                    Ok(diff) if !diff.trim().is_empty() => {
                        state.git_diff_content = diff;
                        state.diff_overlay_visible = true;
                        state.diff_overlay_scroll = 0;
                        state.active_tab = Tab::Git;
                    }
                    Ok(_) => {
                        state.push(ConversationEntry::SystemMsg(
                            "no changes since last checkpoint".to_string(),
                        ));
                    }
                    Err(e) => {
                        state.push(ConversationEntry::SystemMsg(format!("git diff: {e}")));
                    }
                }
            }
        }
        // Enter — submit input
        (KeyModifiers::NONE, KeyCode::Enter) => {
            if state.mode == Mode::AgentRunning || state.mode == Mode::PlanRunning {
                // ignore while agent is running
            } else {
                let input = state.input.trim().to_string();
                if !input.is_empty() {
                    state.input.clear();
                    state.cursor = 0;

                    if input.starts_with("/plan ") || input == "/plan" {
                        let task = input.trim_start_matches("/plan").trim().to_string();
                        if task.is_empty() {
                            state.push(ConversationEntry::SystemMsg(
                                "usage: /plan \"describe the task\"".to_string(),
                            ));
                        } else {
                            generate_and_show_plan(task, state, resolved, ui_tx.clone());
                        }
                    } else if input.starts_with("/quick ") || input == "/quick" {
                        let task = input.trim_start_matches("/quick").trim().to_string();
                        if task.is_empty() {
                            state.push(ConversationEntry::SystemMsg(
                                "usage: /quick \"task\"".to_string(),
                            ));
                        } else {
                            launch_quick(task, state, resolved, verbose, dry_run, ui_tx)?;
                        }
                    } else if input.starts_with('/') {
                        let keep = execute_command(&input, state, resolved, file)?;
                        if !keep {
                            return Ok(false);
                        }
                    } else {
                        launch_agent(input, state, resolved, verbose, dry_run, ui_tx)?;
                    }
                }
            }
        }
        // Tab — cycle focus through attached file chips
        (KeyModifiers::NONE, KeyCode::Tab) => {
            if state.mode == Mode::Normal && !state.attached_files.is_empty() {
                state.focused_chip = Some(match state.focused_chip {
                    None => 0,
                    Some(i) if i + 1 >= state.attached_files.len() => 0,
                    Some(i) => i + 1,
                });
            }
        }
        // Backspace — remove char before cursor, or remove focused chip
        (KeyModifiers::NONE, KeyCode::Backspace) => {
            if state.mode != Mode::AgentRunning {
                if let Some(idx) = state.focused_chip {
                    if idx < state.attached_files.len() {
                        state.attached_files.remove(idx);
                    }
                    state.focused_chip = if state.attached_files.is_empty() {
                        None
                    } else {
                        Some(idx.min(state.attached_files.len() - 1))
                    };
                } else {
                    input_backspace(&mut state.input, &mut state.cursor);
                }
            }
        }
        // Delete — remove char at cursor
        (KeyModifiers::NONE, KeyCode::Delete) => {
            if state.mode != Mode::AgentRunning {
                input_delete_forward(&mut state.input, &mut state.cursor);
            }
        }
        // Ctrl+Backspace — delete word before cursor
        (KeyModifiers::CONTROL, KeyCode::Backspace) | (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            if state.mode != Mode::AgentRunning {
                input_delete_word(&mut state.input, &mut state.cursor);
            }
        }
        // Left arrow — move cursor left
        (KeyModifiers::NONE, KeyCode::Left) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = prev_char_boundary(&state.input, state.cursor);
            }
        }
        // Right arrow — move cursor right
        (KeyModifiers::NONE, KeyCode::Right) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = next_char_boundary(&state.input, state.cursor);
            }
        }
        // Ctrl+Left — jump word left
        (KeyModifiers::CONTROL, KeyCode::Left) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = word_left(&state.input, state.cursor);
            }
        }
        // Ctrl+Right — jump word right
        (KeyModifiers::CONTROL, KeyCode::Right) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = word_right(&state.input, state.cursor);
            }
        }
        // Home / Ctrl+A — go to start of input
        (KeyModifiers::NONE, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = 0;
            }
        }
        // End / Ctrl+E — go to end of input
        (KeyModifiers::NONE, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            if state.mode != Mode::AgentRunning {
                state.cursor = state.input.len();
            }
        }
        // Ctrl+U — clear line before cursor
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            if state.mode != Mode::AgentRunning {
                state.input.drain(..state.cursor);
                state.cursor = 0;
            }
        }
        // Ctrl+K — clear from cursor to end
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            if state.mode != Mode::AgentRunning {
                state.input.truncate(state.cursor);
            }
        }
        // Scroll up
        (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::PageUp) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_add(3);
            } else {
                state.scroll = state.scroll.saturating_add(3);
            }
        }
        // Scroll down
        (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::PageDown) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_sub(3);
            } else {
                state.scroll = state.scroll.saturating_sub(3);
            }
        }
        // Regular char input — insert at cursor
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            if state.mode != Mode::AgentRunning {
                state.focused_chip = None; // typing unfocuses any chip
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                state.input.insert_str(state.cursor, s);
                state.cursor += s.len();

                // `@` triggers file picker
                if c == '@' {
                    let at_offset = state.cursor - 1; // offset of the `@`
                    state.file_picker = Some(FilePickerState {
                        all_files: gather_files(),
                        query: String::new(),
                        selected: 0,
                        at_offset,
                    });
                    state.mode = Mode::FilePicker;
                }

                // `/` at start of input triggers slash autocomplete
                if c == '/' && state.cursor == 1 {
                    state.slash_complete_selected = 0;
                    state.mode = Mode::SlashComplete;
                }
            }
        }
        _ => {}
    }

    Ok(true)
}

// ── Slash command handler ─────────────────────────────────────────────────────

fn execute_command(
    input: &str,
    state: &mut AppState,
    resolved: &mut ResolvedConfig,
    file: &mut ConfigFile,
) -> Result<bool> {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    match parts[0] {
        "/quit" | "/exit" | "/q" => {
            return Ok(false);
        }
        "/help" | "/h" => {
            state.push(ConversationEntry::SystemMsg(
                "Commands: /plan \"task\"  /quick \"task\"  /init  /cd  /profile  /profiles  /ts  /hooks [on|off]  /list-hooks  /undo [n]  /diff  /clear  /sessions  /resume [n]  /rollback [n]  /new  /quit\nCtrl+H  session history  ·  Ctrl+P  command palette  ·  d  open diff overlay\nIn plan review: ↑↓ navigate  e annotate  d clear note  a approve & run  Esc cancel\nIn git repo: press 5 for Git tab · /undo to revert · /diff to review changes".to_string(),
            ));
        }
        "/stats" => {
            let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
            if arg == "reset" {
                match telemetry::clear_all() {
                    Ok(()) => {
                        state.telemetry_history.clear();
                        state.push(ConversationEntry::SystemMsg("telemetry cleared".to_string()));
                    }
                    Err(e) => {
                        state.push(ConversationEntry::SystemMsg(format!("clear failed: {e}")));
                    }
                }
            } else {
                state.active_tab = Tab::Stats;
            }
        }
        "/hooks" => {
            let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
            match arg {
                "off" => {
                    state.hooks_enabled = false;
                    state.push(ConversationEntry::SystemMsg("hooks off".to_string()));
                }
                "on" => {
                    state.hooks_enabled = true;
                    state.push(ConversationEntry::SystemMsg("hooks on".to_string()));
                }
                _ => {
                    let status = if state.hooks_enabled { "on" } else { "off" };
                    state.push(ConversationEntry::SystemMsg(
                        format!("hooks {status}  (usage: /hooks on | /hooks off | /list-hooks)"),
                    ));
                }
            }
        }
        "/list-hooks" => {
            let status = if resolved.hooks_disabled {
                "disabled (hooks_disabled = true in profile)".to_string()
            } else if state.hooks_enabled {
                "on".to_string()
            } else {
                "off (toggled off this session)".to_string()
            };
            let detail = resolved.hooks.detail();
            state.push(ConversationEntry::SystemMsg(
                format!("Hooks — {status}\n{detail}\nEdit ~/.config/parecode/config.toml to change. /hooks on|off to toggle."),
            ));
        }
        "/undo" => {
            match crate::git::GitRepo::open(std::path::Path::new(".")) {
                None => {
                    state.push(ConversationEntry::SystemMsg(
                        "not in a git repository".to_string(),
                    ));
                }
                Some(repo) => match repo.list_checkpoints() {
                    Ok(checkpoints) if checkpoints.is_empty() => {
                        state.push(ConversationEntry::SystemMsg(
                            "no parecode checkpoints found — run a task first".to_string(),
                        ));
                    }
                    Ok(checkpoints) => {
                        state.git_checkpoints = checkpoints;
                        state.undo_picker_selected = 0;
                        state.active_tab = Tab::Git;
                        state.mode = Mode::UndoPicker;
                    }
                    Err(e) => {
                        state.push(ConversationEntry::SystemMsg(format!("git error: {e}")));
                    }
                },
            }
        }
        "/diff" => {
            match crate::git::GitRepo::open(std::path::Path::new(".")) {
                None => {
                    state.push(ConversationEntry::SystemMsg(
                        "not in a git repository".to_string(),
                    ));
                }
                Some(repo) => {
                    let ref_pt = state
                        .last_checkpoint_hash
                        .as_deref()
                        .unwrap_or("HEAD");
                    match repo.diff_full_from(ref_pt) {
                        Ok(diff) if !diff.trim().is_empty() => {
                            state.git_diff_content = diff;
                            state.diff_overlay_visible = true;
                            state.diff_overlay_scroll = 0;
                            state.active_tab = Tab::Git;
                        }
                        Ok(_) => {
                            state.push(ConversationEntry::SystemMsg(
                                "no changes since last checkpoint".to_string(),
                            ));
                        }
                        Err(e) => {
                            state.push(ConversationEntry::SystemMsg(
                                format!("git diff: {e}"),
                            ));
                        }
                    }
                }
            }
        }
        "/clear" => {
            state.entries.clear();
            state.scroll = 0;
        }
        "/sessions" => {
            match sessions::list_sessions() {
                Ok(list) if list.is_empty() => {
                    state.push(ConversationEntry::SystemMsg("no sessions found".to_string()));
                }
                Ok(list) => {
                    let lines: Vec<String> = list
                        .iter()
                        .enumerate()
                        .take(10)
                        .map(|(i, (id, path))| {
                            let turns = sessions::load_session_turns(path)
                                .map(|t| t.len())
                                .unwrap_or(0);
                            format!("  {i}  {id}  ({turns} turns)")
                        })
                        .collect();
                    state.push(ConversationEntry::SystemMsg(
                        format!("Sessions (newest first):\n{}\n/resume <n> to load one", lines.join("\n")),
                    ));
                }
                Err(e) => {
                    state.push(ConversationEntry::SystemMsg(format!("sessions error: {e}")));
                }
            }
        }
        "/resume" => {
            let n: usize = parts
                .get(1)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            match sessions::list_sessions() {
                Ok(list) if list.is_empty() => {
                    state.push(ConversationEntry::SystemMsg("no sessions to resume".to_string()));
                }
                Ok(list) => {
                    if let Some((id, path)) = list.get(n) {
                        match sessions::load_session_turns(path) {
                            Ok(turns) if turns.is_empty() => {
                                state.push(ConversationEntry::SystemMsg(
                                    format!("session {id} has no turns"),
                                ));
                            }
                            Ok(turns) => {
                                let count = turns.len();
                                // Replay turns as display entries so the user can see history
                                for t in &turns {
                                    state.entries.push(ConversationEntry::UserMessage(
                                        t.user_message.clone(),
                                    ));
                                    if !t.agent_response.is_empty() {
                                        state.entries.push(ConversationEntry::AssistantChunk(
                                            t.agent_response.clone(),
                                        ));
                                    }
                                }
                                state.conversation_turns = turns;
                                if let Some(session) = &mut state.session {
                                    session.active_turn = count.saturating_sub(1);
                                }
                                state.push(ConversationEntry::SystemMsg(
                                    format!("✓ resumed {id} ({count} turns) — continue the conversation"),
                                ));
                            }
                            Err(e) => {
                                state.push(ConversationEntry::SystemMsg(
                                    format!("resume error: {e}"),
                                ));
                            }
                        }
                    } else {
                        state.push(ConversationEntry::SystemMsg(
                            format!("no session at index {n} — use /sessions to list"),
                        ));
                    }
                }
                Err(e) => {
                    state.push(ConversationEntry::SystemMsg(format!("sessions error: {e}")));
                }
            }
        }
        "/rollback" => {
            let total = state.conversation_turns.len();
            if total == 0 {
                state.push(ConversationEntry::SystemMsg("no turns to roll back".to_string()));
            } else {
                let target: usize = parts
                    .get(1)
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or_else(|| total.saturating_sub(2));
                let clamped = target.min(total.saturating_sub(1));
                if let Some(session) = &mut state.session {
                    session.active_turn = clamped;
                }
                state.push(ConversationEntry::SystemMsg(
                    format!("rolled back to turn {clamped} (of {total} — next query will use context up to that point)"),
                ));
            }
        }
        "/session-new" | "/new" => {
            state.conversation_turns.clear();
            state.entries.clear();
            state.scroll = 0;
            match sessions::open_session(&cwd_str()) {
                Ok(s) => {
                    state.session = Some(s);
                    sessions::prune_old_sessions(10);
                    state.sidebar_entries = load_sidebar_entries(&state.session);
                    state.push(ConversationEntry::SystemMsg("new session started".to_string()));
                }
                Err(e) => {
                    state.push(ConversationEntry::SystemMsg(format!("session error: {e}")));
                }
            }
        }
        "/init" => {
            let cwd = std::path::PathBuf::from(cwd_str());
            let content = crate::init::run_project_init(&cwd);
            match crate::init::save_conventions(&cwd, &content) {
                Ok(path) => {
                    state.push(ConversationEntry::SystemMsg(
                        format!("✓ conventions written to {}", path.display()),
                    ));
                    // Show a compact preview
                    let preview: String = content.lines().take(10).collect::<Vec<_>>().join("\n");
                    state.push(ConversationEntry::SystemMsg(preview));
                }
                Err(e) => {
                    state.push(ConversationEntry::SystemMsg(format!("/init error: {e}")));
                }
            }
        }
        "/ts" => {
            state.show_timestamps = !state.show_timestamps;
            let s = if state.show_timestamps { "on" } else { "off" };
            state.push(ConversationEntry::SystemMsg(format!("timestamps {s}")));
        }
        "/cd" => {
            let path = parts.get(1).map(|s| s.trim()).unwrap_or("");
            if path.is_empty() {
                state.push(ConversationEntry::SystemMsg("usage: /cd <path>".to_string()));
            } else {
                let expanded = expand_tilde(path);
                match std::env::set_current_dir(&expanded) {
                    Ok(_) => {
                        let cwd = cwd_str();
                        state.push(ConversationEntry::SystemMsg(format!("→ {cwd}")));
                    }
                    Err(e) => {
                        state.push(ConversationEntry::SystemMsg(format!("cd: {e}")));
                    }
                }
            }
        }
        "/profiles" => {
            *file = crate::config::ConfigFile::load()?;
            let mut entries: Vec<String> = file
                .profiles
                .iter()
                .map(|(name, p)| {
                    let marker = if name == &file.default_profile { " ←" } else { "" };
                    format!("  {name}{marker}  {}", p.model)
                })
                .collect();
            entries.sort();
            state.push(ConversationEntry::SystemMsg(format!("Profiles:\n{}", entries.join("\n"))));
        }
        "/profile" => {
            let name = parts.get(1).map(|s| s.trim()).unwrap_or("");
            if name.is_empty() {
                state.push(ConversationEntry::SystemMsg(
                    "usage: /profile <name>  (use /profiles to list)".to_string(),
                ));
            } else {
                *file = crate::config::ConfigFile::load()?;
                if file.profiles.contains_key(name) {
                    *resolved = crate::config::ResolvedConfig::resolve(file, Some(name), None, None, None);
                    state.profile = resolved.profile_name.clone();
                    state.model = resolved.model.clone();
                    state.context_tokens = resolved.context_tokens;
                    state.cost_per_mtok_input = resolved.cost_per_mtok_input;
                    state.push(ConversationEntry::SystemMsg(format!(
                        "✓ {} · {} · {}k ctx",
                        resolved.profile_name, resolved.model, resolved.context_tokens / 1000
                    )));
                } else {
                    state.push(ConversationEntry::SystemMsg(format!(
                        "profile '{name}' not found — use /profiles to list"
                    )));
                }
            }
        }
        _ => {
            state.push(ConversationEntry::SystemMsg(
                format!("unknown command: {}  (try /help or Ctrl+P)", parts[0]),
            ));
        }
    }
    Ok(true)
}

// ── Hook resolver ─────────────────────────────────────────────────────────────

/// Determine which hooks to use for this run.
/// By startup time, `resolved.hooks` is always populated (either from config
/// or from the bootstrap write). This just gates on hooks_enabled.
fn resolve_hooks(resolved: &ResolvedConfig, hooks_enabled: bool) -> crate::hooks::HookConfig {
    if !hooks_enabled || resolved.hooks_disabled {
        return crate::hooks::HookConfig::default();
    }
    resolved.hooks.clone()
}

// ── Agent launcher ────────────────────────────────────────────────────────────

fn launch_agent(
    task: String,
    state: &mut AppState,
    resolved: &ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    state.push(ConversationEntry::UserMessage(task.clone()));
    state.current_task_preview = task.lines().next().unwrap_or(&task).chars().take(80).collect();
    state.mode = Mode::AgentRunning;
    state.ctx_used = 0;
    state.ctx_compressed = false;

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    state.cancel_tx = Some(cancel_tx);

    // Build client + config
    let mut client = Client::new(resolved.endpoint.clone(), resolved.model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }
    let resolved_hooks = resolve_hooks(resolved, state.hooks_enabled);
    let agent_config = AgentConfig {
        verbose,
        dry_run,
        context_tokens: resolved.context_tokens,
        _profile_name: resolved.profile_name.clone(),
        _model: resolved.model.clone(),
        _show_timestamps: state.show_timestamps,
        mcp: state.mcp.clone(),
        hooks: std::sync::Arc::new(resolved_hooks),
        hooks_enabled: state.hooks_enabled,
        auto_commit: resolved.auto_commit,
        auto_commit_prefix: resolved.auto_commit_prefix.clone(),
        git_context: resolved.git_context,
    };

    let attached: Vec<(String, String)> = state.attached_files
        .iter()
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect();

    // Build prior context from completed turns (up to the active_turn rollback pointer)
    let active_limit = state
        .session
        .as_ref()
        .map(|s| s.active_turn + 1) // turns[0..=active_turn]
        .unwrap_or(usize::MAX);
    let visible_turns: Vec<&ConversationTurn> = state
        .conversation_turns
        .iter()
        .filter(|t| t.turn_index < active_limit)
        .collect();
    // Clone needed to move into the spawn closure
    let visible_owned: Vec<ConversationTurn> = visible_turns.iter().map(|t| (*t).clone()).collect();
    let prior_context = sessions::build_prior_context(&visible_owned);

    // Reset collectors for this new run
    state.collecting_response.clear();
    state.collecting_tools.clear();

    tokio::spawn(async move {
        tokio::select! {
            result = crate::agent::run_tui(&task, &client, &agent_config, attached, prior_context, ui_tx.clone()) => {
                if let Err(e) = result {
                    let _ = ui_tx.send(UiEvent::AgentError(e.to_string()));
                }
            }
            _ = async move {
                let _ = cancel_rx.await;
            } => {
                let _ = ui_tx.send(UiEvent::AgentError("cancelled".to_string()));
            }
        }
    });

    Ok(())
}

// ── Quick mode launcher ───────────────────────────────────────────────────────

fn launch_quick(
    task: String,
    state: &mut AppState,
    resolved: &ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    state.push(ConversationEntry::UserMessage(format!("⚡ {task}")));
    state.current_task_preview = task.lines().next().unwrap_or(&task).chars().take(80).collect();
    state.mode = Mode::AgentRunning;
    state.ctx_used = 0;
    state.ctx_compressed = false;

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    state.cancel_tx = Some(cancel_tx);

    let mut client = Client::new(resolved.endpoint.clone(), resolved.model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }
    // Quick mode: no on_edit hooks (single shot, no mutation loop)
    let agent_config = AgentConfig {
        verbose,
        dry_run,
        context_tokens: resolved.context_tokens,
        _profile_name: resolved.profile_name.clone(),
        _model: resolved.model.clone(),
        _show_timestamps: state.show_timestamps,
        mcp: state.mcp.clone(),
        hooks: std::sync::Arc::new(crate::hooks::HookConfig::default()),
        hooks_enabled: false,
        auto_commit: false,
        auto_commit_prefix: String::new(),
        git_context: false,
    };

    state.collecting_response.clear();
    state.collecting_tools.clear();

    tokio::spawn(async move {
        tokio::select! {
            result = crate::agent::run_quick(&task, &client, &agent_config, ui_tx.clone()) => {
                if let Err(e) = result {
                    let _ = ui_tx.send(UiEvent::AgentError(e.to_string()));
                }
            }
            _ = async move {
                let _ = cancel_rx.await;
            } => {
                let _ = ui_tx.send(UiEvent::AgentError("cancelled".to_string()));
            }
        }
    });

    Ok(())
}

// ── Plan generation ───────────────────────────────────────────────────────────

/// Spawn a background task to generate the plan, sending results back via UiEvent.
fn generate_and_show_plan(
    task: String,
    state: &mut AppState,
    resolved: &ResolvedConfig,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) {
    // Use planner_model if configured, otherwise fall back to the regular model
    let plan_model = resolved.planner_model
        .clone()
        .unwrap_or_else(|| resolved.model.clone());

    let planner_note = if resolved.planner_model.is_some() {
        format!(" via {plan_model}")
    } else {
        String::new()
    };
    state.push(ConversationEntry::SystemMsg(
        format!("⟳ planning{planner_note}: {task}"),
    ));
    // Use AgentRunning mode to get spinner while generating
    state.mode = Mode::AgentRunning;
    let mut client = Client::new(resolved.endpoint.clone(), plan_model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }

    let context_files: Vec<(String, String)> = state.attached_files
        .iter()
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect();

    let project = cwd_str()
        .split('/')
        .last()
        .unwrap_or("project")
        .to_string();

    // Build symbol index from the project — zero model calls, pure file scan
    let index = crate::index::SymbolIndex::build(std::path::Path::new("."), 500);

    tokio::spawn(async move {
        match plan::generate_plan(&task, &client, &project, &context_files, &index).await {
            Ok(generated_plan) => {
                // Save plan to disk: JSON for machine use, Markdown for human reading
                let _ = plan::save_plan(&generated_plan);
                plan::write_plan_to_disk(&generated_plan);
                let _ = ui_tx.send(UiEvent::PlanReady(generated_plan));
            }
            Err(e) => {
                let _ = ui_tx.send(UiEvent::PlanGenerateFailed(e.to_string()));
            }
        }
    });
}

// ── Plan execution ─────────────────────────────────────────────────────────────

/// Spawn a background task to execute all plan steps sequentially.
/// All state updates are communicated back to the TUI via UiEvent.
fn launch_plan(
    mut active_plan: Plan,
    state: &AppState,
    resolved: &ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) {
    let mut client = Client::new(resolved.endpoint.clone(), resolved.model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }
    let resolved_hooks = resolve_hooks(resolved, state.hooks_enabled);
    let agent_config = AgentConfig {
        verbose,
        dry_run,
        context_tokens: resolved.context_tokens,
        _profile_name: resolved.profile_name.clone(),
        _model: resolved.model.clone(),
        _show_timestamps: state.show_timestamps,
        mcp: state.mcp.clone(),
        hooks: std::sync::Arc::new(resolved_hooks),
        hooks_enabled: state.hooks_enabled,
        auto_commit: resolved.auto_commit,
        auto_commit_prefix: resolved.auto_commit_prefix.clone(),
        git_context: resolved.git_context,
    };

    tokio::spawn(async move {
        let total = active_plan.steps.len();
        // Accumulates (description, summary) for each completed step
        let mut prior_summaries: Vec<(String, String)> = Vec::new();

        // ── Plan-level git checkpoint ─────────────────────────────────────────
        // Capture the state before any step runs — used for cumulative diff.
        let plan_checkpoint: Option<String> = if agent_config.git_context {
            std::env::current_dir().ok().and_then(|cwd| {
                crate::git::GitRepo::open(&cwd).and_then(|repo| {
                    let summary = format!(
                        "plan: {}",
                        active_plan.task.chars().take(50).collect::<String>()
                    );
                    repo.checkpoint(&summary).ok()
                })
            })
        } else {
            None
        };

        for step_idx in 0..total {
            let step_snapshot = active_plan.steps[step_idx].clone();
            let desc = step_snapshot.description.clone();

            // ── Per-step git checkpoint ───────────────────────────────────────
            let step_checkpoint: Option<String> = if agent_config.git_context {
                std::env::current_dir().ok().and_then(|cwd| {
                    crate::git::GitRepo::open(&cwd).and_then(|repo| {
                        let summary = format!(
                            "plan step {}/{}: {}",
                            step_idx + 1,
                            total,
                            desc.chars().take(40).collect::<String>()
                        );
                        repo.checkpoint(&summary).ok()
                    })
                })
            } else {
                None
            };

            let _ = ui_tx.send(UiEvent::PlanStepStart {
                index: step_idx,
                total,
                desc: desc.clone(),
            });

            // Run the step (agent call)
            let step_result = plan::execute_step(
                &step_snapshot,
                &client,
                &agent_config,
                &prior_summaries,
                ui_tx.clone(),
            ).await;

            // Run verification
            let verify_result = match &step_result {
                Ok(()) => plan::verify_step(&step_snapshot),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            };

            match verify_result {
                Ok(()) => {
                    active_plan.steps[step_idx].status = StepStatus::Pass;
                    // Inspect modified files to record what actually changed
                    let summary = plan::summarise_completed_step(&step_snapshot);
                    active_plan.steps[step_idx].completed_summary = Some(summary.clone());
                    prior_summaries.push((desc, summary));
                    let _ = ui_tx.send(UiEvent::PlanStepDone {
                        index: step_idx,
                        passed: true,
                        error: None,
                    });
                    // on_plan_step_done hooks — shown in TUI
                    if agent_config.hooks_enabled {
                        for cmd in &agent_config.hooks.on_plan_step_done {
                            let hr = crate::hooks::run_hook(cmd).await;
                            let _ = ui_tx.send(UiEvent::HookOutput {
                                event: "on_plan_step_done".to_string(),
                                output: hr.output,
                                exit_code: hr.exit_code,
                            });
                        }
                    }
                    // ── Per-step diff notification ────────────────────────────
                    if agent_config.git_context {
                        if let Some(cwd) = std::env::current_dir().ok() {
                            if let Some(repo) = crate::git::GitRepo::open(&cwd) {
                                let ref_pt = step_checkpoint.as_deref().unwrap_or("HEAD");
                                if let Ok(stat) = repo.diff_stat_from(ref_pt) {
                                    if !stat.trim().is_empty() {
                                        let files_changed =
                                            stat.lines().filter(|l| l.contains('|')).count();
                                        let _ = ui_tx.send(UiEvent::GitChanges {
                                            stat: stat.trim().to_string(),
                                            checkpoint_hash: step_checkpoint.clone(),
                                            files_changed,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    active_plan.steps[step_idx].status = StepStatus::Fail;
                    active_plan.status = crate::plan::PlanStatus::Failed;
                    let _ = plan::save_plan(&active_plan);
                    let _ = ui_tx.send(UiEvent::PlanStepDone {
                        index: step_idx,
                        passed: false,
                        error: Some(e.to_string()),
                    });
                    let _ = ui_tx.send(UiEvent::PlanFailed {
                        step: step_idx,
                        error: e.to_string(),
                    });
                    return;
                }
            }
        }

        // All steps passed
        active_plan.status = crate::plan::PlanStatus::Complete;
        let _ = plan::save_plan(&active_plan);

        // ── Cumulative diff + auto-commit ─────────────────────────────────────
        if agent_config.git_context {
            if let Some(cwd) = std::env::current_dir().ok() {
                if let Some(repo) = crate::git::GitRepo::open(&cwd) {
                    let ref_pt = plan_checkpoint.as_deref().unwrap_or("HEAD");
                    if let Ok(stat) = repo.diff_stat_from(ref_pt) {
                        if !stat.trim().is_empty() {
                            let files_changed =
                                stat.lines().filter(|l| l.contains('|')).count();
                            let cumulative_stat =
                                format!("Plan complete — cumulative changes:\n{}", stat.trim());
                            let _ = ui_tx.send(UiEvent::GitChanges {
                                stat: cumulative_stat,
                                checkpoint_hash: plan_checkpoint.clone(),
                                files_changed,
                            });
                        }
                    }
                    if agent_config.auto_commit {
                        let summary: String =
                            active_plan.task.chars().take(72).collect();
                        let msg =
                            format!("{}{}", agent_config.auto_commit_prefix, summary);
                        match repo.auto_commit(&msg) {
                            Ok(()) => {
                                let _ = ui_tx.send(UiEvent::GitAutoCommit { message: msg });
                            }
                            Err(e) => {
                                let _ = ui_tx
                                    .send(UiEvent::GitError(format!("auto-commit: {e}")));
                            }
                        }
                    }
                }
            }
        }

        let _ = ui_tx.send(UiEvent::PlanComplete { total });
    });
}

// ── Sidebar data loading ──────────────────────────────────────────────────────

fn load_sidebar_entries(current_session: &Option<sessions::Session>) -> Vec<SidebarEntry> {
    let current_id = current_session.as_ref().map(|s| s.id.as_str()).unwrap_or("");
    let all = sessions::list_sessions().unwrap_or_default();
    all.into_iter()
        // Skip empty (zero-byte) files — sessions with no turns yet
        .filter(|(_, path)| std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false))
        .take(10)
        .map(|(id, path)| {
            // Turn count — load cheaply by counting lines
            let turn_count = std::fs::read_to_string(&path)
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            // Preview — first user_message from first line
            let preview = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.lines().next().map(|l| l.to_string()))
                .and_then(|l| serde_json::from_str::<sessions::ConversationTurn>(&l).ok())
                .map(|t| {
                    let msg = t.user_message.lines().next().unwrap_or("").to_string();
                    msg.chars().take(26).collect::<String>()
                })
                .unwrap_or_default();
            // Project = part after first underscore in id
            let project = id.splitn(2, '_').nth(1).unwrap_or(&id).chars().take(16).collect();
            // Timestamp from unix prefix
            let timestamp = id.splitn(2, '_').next()
                .and_then(|ts| ts.parse::<i64>().ok())
                .map(|ts| {
                    let dt = chrono::DateTime::from_timestamp(ts, 0)
                        .unwrap_or_default()
                        .with_timezone(&chrono::Local);
                    dt.format("%b %d %H:%M").to_string()
                })
                .unwrap_or_default();
            let is_current = id == current_id;
            SidebarEntry { id, path, project, turn_count, preview, timestamp, is_current }
        })
        .collect()
}

// ── Small helpers ─────────────────────────────────────────────────────────────

pub fn cwd_str() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        path.replacen("~", &home, 1)
    } else {
        path.to_string()
    }
}

// ── Tool action summariser ────────────────────────────────────────────────────

/// Build a compact action label for session history, e.g. `edit_file(src/foo.rs)`.
/// Extracts the path from args_summary (format: `path="src/foo.rs", ...`).
fn compact_tool_action(name: &str, args_summary: &str) -> String {
    // Try to extract path="..." from the args_summary string
    let path = args_summary
        .split(',')
        .find_map(|part| {
            let part = part.trim();
            if part.starts_with("path=") {
                let val = part.trim_start_matches("path=").trim_matches('"');
                // Strip leading "./" for cleanliness
                Some(val.trim_start_matches("./").to_string())
            } else {
                None
            }
        });
    match path {
        Some(p) if !p.is_empty() => format!("{name}({p})"),
        _ => name.to_string(),
    }
}

// ── Input editing helpers ─────────────────────────────────────────────────────

/// Remove the character immediately before the cursor (UTF-8 safe).
fn input_backspace(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = prev_char_boundary(input, *cursor);
    input.drain(prev..*cursor);
    *cursor = prev;
}

/// Delete the character at the cursor position.
fn input_delete_forward(input: &mut String, cursor: &mut usize) {
    if *cursor >= input.len() {
        return;
    }
    let next = next_char_boundary(input, *cursor);
    input.drain(*cursor..next);
}

/// Delete the word immediately before the cursor (stops at whitespace boundary).
fn input_delete_word(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = word_left(input, *cursor);
    input.drain(start..*cursor);
    *cursor = start;
}

/// Previous UTF-8 char boundary before `pos`.
fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Next UTF-8 char boundary after `pos`.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p <= s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p.min(s.len())
}

/// Jump to the start of the previous word (skip trailing spaces, then the word).
fn word_left(s: &str, mut pos: usize) -> usize {
    let bytes = s.as_bytes();
    // Skip whitespace to the left
    while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    // Skip non-whitespace to the left
    while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    pos
}

/// Jump past the end of the next word to the right.
fn word_right(s: &str, mut pos: usize) -> usize {
    let bytes = s.as_bytes();
    let len = s.len();
    // Skip whitespace to the right
    while pos < len && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    // Skip non-whitespace to the right
    while pos < len && !bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}
