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
pub mod input_box;

use input_box::{InputBox, InputAction};

use std::io;

use anyhow::Result;
use crossterm::{
    event::{
        Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
        EnableBracketedPaste, DisableBracketedPaste,
        EnableMouseCapture, DisableMouseCapture,
        MouseEventKind,
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

#[derive(Debug)]
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
        _input: u32,
        _output: u32,
        total_input: u32,
        total_output: u32,
        tool_calls: usize,
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
    /// System message from background tasks (e.g. update check)
    SystemMsg(String),
    /// Model is asking the user a clarifying question — pause agent until answered
    AskUser {
        question: String,
        reply_tx: tokio::sync::oneshot::Sender<String>,
    },
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
    /// The model asked the user a question (displayed as a prompt)
    AskUser(String),
    /// The user's reply to an ask_user question
    AskReply(String),
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
    ProfilePicker,  // Interactive profile picker overlay in Config tab
    AskingUser,     // Model asked a question, waiting for user's typed answer
    HookWizard,     // First-run hook setup wizard
}

// ── Hook wizard state ─────────────────────────────────────────────────────────

/// Which field of the hook setup form is currently being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardStep {
    EnterName,
    EnterOnEdit,
    EnterOnTaskDone,
    EnterOnPlanStepDone,
    EnterOnSessionStart,
    EnterOnSessionEnd,
    Confirm,
}

#[derive(Debug, Clone)]
pub struct HookWizardState {
    pub step: WizardStep,
    pub name_input: String,
    pub on_edit_input: String,
    pub on_task_done_input: String,
    pub on_plan_step_done_input: String,
    pub on_session_start_input: String,
    pub on_session_end_input: String,
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

        // Skip .git, target dir, node_modules, __pycache__ — allow .parecode and other dotfiles
        if name_str == ".git"
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
    /// tui-textarea powered input box for Normal / AskingUser input
    pub input_box: InputBox,
    /// Backing string for special modes (SlashComplete, FilePicker) that manipulate
    /// the input directly (not via tui-textarea). Also used as read cache for tab-switch
    /// guards (`input_box.is_empty()`).
    pub input: String,
    pub cursor: usize,        // byte offset in input (used only by special modes)
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
    /// MCP server names configured for this profile (for Config tab display)
    pub mcp_server_names: Vec<String>,

    // ── Config tab editable state ─────────────────────────────────────────────
    /// Whether auto-commit is enabled (mirrors resolved config, for Config tab display)
    pub auto_commit: bool,
    /// Auto-commit prefix string
    #[allow(dead_code)]
    pub auto_commit_prefix: String,
    /// Whether git context/checkpoints are enabled (mirrors resolved config)
    pub git_context_enabled: bool,

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
    /// Set to true to shell out to $EDITOR on config.toml; event loop handles it
    pub wants_editor: bool,
    /// Scroll offset for the Config tab content
    pub config_scroll: usize,
    /// Selected index in the profile picker overlay
    pub profile_picker_selected: usize,
    /// Sorted profile names for the picker (populated when picker opens)
    pub profile_picker_entries: Vec<(String, String)>,  // (name, model)
    /// Oneshot sender to reply to an ask_user tool call (Some while Mode::AskingUser)
    pub pending_ask_reply: Option<tokio::sync::oneshot::Sender<String>>,
    /// Active hook config name (e.g. "rust" from [hooks.rust]), persisted in config
    pub active_hook_preset: Option<String>,
    /// Available hook config names from config (for `/hooks list`)
    pub available_hook_presets: Vec<String>,
    /// Hook setup wizard state (Some when Mode::HookWizard)
    pub hook_wizard: Option<HookWizardState>,
    /// Persistent project graph — loaded/built at startup, injected into every agent call.
    /// None only before the first build completes (should not happen in normal flow).
    pub project_graph: Option<crate::pie::ProjectGraph>,
    /// PIE Phase 2 narrative — architecture summary + cluster summaries + conventions.
    /// Generated once on cold startup via one model call; warm runs load instantly from disk.
    pub project_narrative: Option<crate::narrative::ProjectNarrative>,
    /// PIE Phase 3 context weights — tracks which files are useful vs wasted per task.
    pub context_weights: crate::context_weights::ContextWeights,
}

impl AppState {
    pub fn new(resolved: &ResolvedConfig, show_timestamps: bool, mcp: Arc<McpClient>) -> Self {
        Self {
            entries: Vec::new(),
            input_box: InputBox::new(),
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
            hooks_config: if !resolved.active_hook_config.is_empty() {
                resolved.active_hook_config.clone()
            } else {
                resolved.hooks.clone()
            },
            hooks_disabled_profile: resolved.hooks_disabled,
            mcp_server_names: resolved.mcp_servers.iter().map(|s| s.name.clone()).collect(),
            auto_commit: resolved.auto_commit,
            auto_commit_prefix: resolved.auto_commit_prefix.clone(),
            git_context_enabled: resolved.git_context,
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
            wants_editor: false,
            config_scroll: 0,
            profile_picker_selected: 0,
            profile_picker_entries: Vec::new(),
            pending_ask_reply: None,
            active_hook_preset: resolved.active_hooks.clone(),
            available_hook_presets: resolved.available_hooks.clone(),
            hook_wizard: None,
            project_graph: None,     // populated during splash in event_loop
            project_narrative: None, // populated during splash in event_loop
            context_weights: crate::context_weights::ContextWeights::load(),
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
                // Clear in-flight counters — final totals are in the record
                self.stats.clear_inflight();
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

                // PIE Phase 3: extract summary, record task, adjust context weights
                {
                    let summary = crate::task_memory::extract_summary(&self.collecting_response);
                    let files_modified = extract_modified_files(&self.collecting_tools);
                    let files_in_context: Vec<String> = self.attached_files
                        .iter()
                        .map(|f| f.path.clone())
                        .collect();
                    let task_record = crate::task_memory::TaskRecord::new(
                        &self.current_task_preview,
                        "solved",
                        files_modified.clone(),
                        &summary,
                        input_tokens + output_tokens,
                        files_in_context.clone(),
                    );
                    crate::task_memory::append_record(&task_record);
                    self.context_weights.adjust(&files_modified, &files_in_context);
                    self.context_weights.save();
                }

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
                // Record partial token usage before clearing — if the agent
                // errored mid-task we still want those tokens counted in
                // session / daily / all-time stats.
                let inf_in = self.stats.inflight_input_tokens;
                let inf_out = self.stats.inflight_output_tokens;
                if inf_in > 0 || inf_out > 0 {
                    let session_id = self.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
                    let record = self.stats.record_task(
                        &session_id,
                        "",
                        &format!("[error] {}", self.current_task_preview),
                        inf_in,
                        inf_out,
                        self.stats.inflight_tool_calls,
                        0,
                        0,
                        &self.model.clone(),
                        &self.profile.clone(),
                    );
                    telemetry::append_record(&record);
                }
                self.stats.clear_inflight();
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
            UiEvent::TokenStats { _input: _, _output: _, total_input, total_output, tool_calls } => {
                // Always update in-flight counters so stats tab shows live usage
                self.stats.update_inflight(total_input, total_output, tool_calls);

                // Periodically flush a partial telemetry record (every 30s)
                // so token usage survives crashes / cancellation
                if self.stats.should_flush(30) {
                    let session_id = self.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
                    let partial = crate::telemetry::TaskRecord {
                        timestamp: chrono::Utc::now().timestamp(),
                        session_id,
                        cwd: String::new(),
                        task_preview: format!("[in-flight] {}", self.current_task_preview.chars().take(70).collect::<String>()),
                        input_tokens: total_input,
                        output_tokens: total_output,
                        tool_calls,
                        compressed_count: 0,
                        compression_ratio: 0.0,
                        duration_secs: 0,
                        model: self.model.clone(),
                        profile: self.profile.clone(),
                    };
                    crate::telemetry::append_record(&partial);
                }
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
            UiEvent::SystemMsg(msg) => {
                self.push(ConversationEntry::SystemMsg(msg));
            }
            UiEvent::AskUser { question, reply_tx } => {
                self.push(ConversationEntry::AskUser(question));
                self.pending_ask_reply = Some(reply_tx);
                self.mode = Mode::AskingUser;
                // Clear input so the user starts with a fresh prompt
                self.input_box.clear();
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
        PaletteCommand { key: "/hooks",       label: "Toggle hooks on/off or switch preset (/hooks on|off|list|<preset>)" },
        PaletteCommand { key: "/list-hooks",  label: "Show all configured hooks and their status" },
        PaletteCommand { key: "/pie",         label: "Show PIE status: graph health, narrative, task memory, context weights" },
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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

// ── Main TUI run loop ─────────────────────────────────────────────────────────

pub async fn run(
    file: ConfigFile,
    initial: ResolvedConfig,
    verbose: bool,
    dry_run: bool,
    show_timestamps: bool,
    update_notice: tokio::task::JoinHandle<Option<(String, String)>>,
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
        update_notice,
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
    update_notice: tokio::task::JoinHandle<Option<(String, String)>>,
) -> Result<()> {
    let mcp = McpClient::new(&resolved.mcp_servers).await;

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
    } else if let Some(name) = &resolved.active_hooks {
        let summary = resolved.active_hook_config.summary()
            .unwrap_or_else(|| "no commands configured".to_string());
        state.push(ConversationEntry::SystemMsg(
            format!("⚙ hooks.{name}  {summary}  (/list-hooks for details)"),
        ));
    } else if let Some(summary) = resolved.hooks.summary() {
        state.push(ConversationEntry::SystemMsg(format!("⚙ hooks  {summary}  (/list-hooks for details)")));
    }

    // ── Update notification ───────────────────────────────────────────────────
    if let Ok(Some((current, latest))) = update_notice.await {
        state.push(ConversationEntry::SystemMsg(
            format!("⬆ update available: parecode {current} → {latest}  (run `parecode --update`)"),
        ));
    }

    // ── PIE warm status ───────────────────────────────────────────────────────
    let graph_path = std::path::Path::new(".parecode/project.graph");
    if graph_path.exists() {
        if let Ok(content) = std::fs::read_to_string(graph_path) {
            if let Ok(g) = serde_json::from_str::<crate::pie::ProjectGraph>(&content) {
                let cluster_count = g.clusters.len();
                let file_count: usize = g.clusters.iter().map(|c| c.files.len()).sum();
                state.push(ConversationEntry::SystemMsg(
                    format!("◈ PIE  {cluster_count} clusters · {file_count} files · warm"),
                ));
            }
        }
    }

    // Open session (resumes existing or creates new) — loads turns and sets active_turn
    let cwd = cwd_str();
    match sessions::open_session(&cwd) {
        Ok(session) => {
            // Sync conversation_turns from the loaded session for context injection
            state.conversation_turns = session._turns.clone();

            // Replay loaded turns as display entries so history is visible
            if !state.conversation_turns.is_empty() {
                state.session_resumed = true;
                for t in &state.conversation_turns {
                    state.entries.push(ConversationEntry::UserMessage(t.user_message.clone()));
                    if !t.agent_response.is_empty() {
                        state.entries.push(ConversationEntry::AssistantChunk(t.agent_response.clone()));
                    }
                }
                let count = state.conversation_turns.len();
                state.push(ConversationEntry::SystemMsg(
                    format!("↩ resumed session · {count} turn{} · /new for a fresh start",
                        if count == 1 { "" } else { "s" }),
                ));
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

    // ── /init auto-prompt for new projects ──────────────────────────────────
    // If this project directory doesn't have .parecode/conventions.md, offer to create it.
    {
        let cwd_path = std::path::Path::new(&cwd);
        let conventions = cwd_path.join(".parecode/conventions.md");
        if !conventions.exists() {
            // Check if this looks like a project directory
            let markers = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod",
                           "Makefile", "CMakeLists.txt"];
            if markers.iter().any(|m| cwd_path.join(m).exists()) {
                let content = crate::init::run_project_init(cwd_path);
                match crate::init::save_conventions(cwd_path, &content) {
                    Ok(path) => {
                        state.push(ConversationEntry::SystemMsg(
                            format!("✓ project conventions detected → {}", path.display()),
                        ));
                    }
                    Err(e) => {
                        state.push(ConversationEntry::SystemMsg(
                            format!("⚠ could not write conventions: {e}"),
                        ));
                    }
                }
            }
        }
    }

    // ── Version check (background, non-blocking) ─────────────────────────────
    let update_tx = ui_tx.clone();
    tokio::spawn(async move {
        if let Some((_current, latest)) = crate::setup::check_for_update().await {
            let _ = update_tx.send(UiEvent::SystemMsg(
                format!("update available: v{latest} — see https://github.com/PartTimer1996/parecode/releases"),
            ));
        }
    });

    let mut crossterm_events = EventStream::new();
    let mut ticker = tokio::time::interval(tokio::time::Duration::from_millis(120));

    // ── Splash + PIE build (animated) ────────────────────────────────────────
    // Spinner redraws every 120ms. Minimum display: 1400ms.
    // Cold narrative (one model call) can take 5–10s; spinner keeps it alive.
    let graph_is_warm = std::path::Path::new(".parecode/project.graph").exists();
    let narrative_is_warm = std::path::Path::new(".parecode/narrative.json").exists();

    // Start graph build on thread pool immediately
    let graph_task = tokio::task::spawn_blocking(|| {
        crate::pie::ProjectGraph::load_or_build(std::path::Path::new("."), 500)
    });

    let mut splash_frame: u8 = 0;
    let mut splash_ticker = tokio::time::interval(tokio::time::Duration::from_millis(120));
    let splash_start = std::time::Instant::now();

    // Phase 1: spin while graph builds (minimum 1400ms so the logo is legible)
    let graph_status = if graph_is_warm { "loading project graph…" } else { "indexing project…" };
    loop {
        splash_ticker.tick().await;
        splash_frame = splash_frame.wrapping_add(1);
        terminal.draw(|f| render::draw_splash(f, graph_status, splash_frame))?;
        let elapsed = splash_start.elapsed().as_millis();
        if graph_task.is_finished() && elapsed >= 1400 {
            break;
        }
    }
    let graph_result = graph_task.await;

    if let Ok((graph, _was_warm)) = graph_result {
        // Phase 2: load or generate narrative — keep spinning, no time cap.
        let mut nar_client = Client::new(resolved.endpoint.clone(), resolved.model.clone());
        if let Some(ref key) = resolved.api_key {
            nar_client.set_api_key(key.clone());
        }
        let narrative_task = tokio::spawn(crate::narrative::ProjectNarrative::load_or_generate(
            graph.clone(),
            nar_client,
            std::path::Path::new(".").to_path_buf(),
        ));

        let nar_status = if narrative_is_warm { "loading project narrative…" } else { "generating project narrative…" };
        let narrative = loop {
            splash_ticker.tick().await;
            splash_frame = splash_frame.wrapping_add(1);
            terminal.draw(|f| render::draw_splash(f, nar_status, splash_frame))?;
            if narrative_task.is_finished() {
                break narrative_task.await.ok().flatten();
            }
        };

        state.project_narrative = narrative;
        state.project_graph = Some(graph);
    }

    terminal.draw(|f| render::draw(f, &mut state))?;

    loop {
        tokio::select! {
            // ── Animation tick ────────────────────────────────────────────────
            _ = ticker.tick() => {
                if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) {
                    state.spinner_tick = state.spinner_tick.wrapping_add(1);
                    terminal.draw(|f| render::draw(f, &mut state))?;
                }
            }

            // ── Drain UI events from agent ────────────────────────────────────
            Some(ev) = ui_rx.recv() => {
                state.apply_event(ev);
                terminal.draw(|f| render::draw(f, &mut state))?;
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
                    Event::Paste(text) => {
                        if state.mode != Mode::AgentRunning && state.mode != Mode::PlanRunning {
                            if matches!(state.mode, Mode::Normal | Mode::AskingUser) {
                                // Insert via input_box (preserves newlines)
                                state.input_box.insert_str(&text);
                            } else {
                                // Special modes use state.input directly
                                state.input.insert_str(state.cursor, &text);
                                state.cursor += text.len();
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                if state.active_tab == Tab::Stats {
                                    state.stats_scroll = state.stats_scroll.saturating_add(3);
                                } else {
                                    state.scroll = state.scroll.saturating_add(3);
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if state.active_tab == Tab::Stats {
                                    state.stats_scroll = state.stats_scroll.saturating_sub(3);
                                } else {
                                    state.scroll = state.scroll.saturating_sub(3);
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }

                // Shell out to $EDITOR if requested by Config tab
                if state.wants_editor {
                    state.wants_editor = false;
                    let config_file_path = crate::config::config_path();
                    let editor = std::env::var("EDITOR")
                        .or_else(|_| std::env::var("VISUAL"))
                        .unwrap_or_else(|_| "vi".to_string());

                    // Suspend TUI
                    disable_raw_mode()?;
                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

                    // Run editor
                    let status = std::process::Command::new(&editor)
                        .arg(&config_file_path)
                        .status();

                    // Resume TUI
                    enable_raw_mode()?;
                    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
                    terminal.clear()?;

                    match status {
                        Ok(s) if s.success() => {
                            // Reload config from disk
                            match crate::config::ConfigFile::load() {
                                Ok(new_file) => {
                                    let profile_name = state.profile.clone();
                                    resolved = crate::config::ResolvedConfig::resolve(
                                        &new_file, Some(&profile_name), None, None, None,
                                    );
                                    file = new_file;
                                    // Sync display state from re-resolved config
                                    state.profile = resolved.profile_name.clone();
                                    state.model = resolved.model.clone();
                                    state.context_tokens = resolved.context_tokens;
                                    state.endpoint = resolved.endpoint.clone();
                                    state.cost_per_mtok_input = resolved.cost_per_mtok_input;
                                    state.hooks_config = if !resolved.active_hook_config.is_empty() {
                                        resolved.active_hook_config.clone()
                                    } else {
                                        resolved.hooks.clone()
                                    };
                                    state.active_hook_preset = resolved.active_hooks.clone();
                                    state.hooks_disabled_profile = resolved.hooks_disabled;
                                    state.mcp_server_names = resolved.mcp_servers.iter().map(|s| s.name.clone()).collect();
                                    state.auto_commit = resolved.auto_commit;
                                    state.auto_commit_prefix = resolved.auto_commit_prefix.clone();
                                    state.git_context_enabled = resolved.git_context;
                                    state.push(ConversationEntry::SystemMsg(
                                        "✓ config reloaded".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    state.push(ConversationEntry::SystemMsg(
                                        format!("✗ failed to reload config: {e}"),
                                    ));
                                }
                            }
                        }
                        Ok(_) => {
                            state.push(ConversationEntry::SystemMsg(
                                "editor exited with error — config not reloaded".to_string(),
                            ));
                        }
                        Err(e) => {
                            state.push(ConversationEntry::SystemMsg(
                                format!("✗ failed to launch editor '{editor}': {e}"),
                            ));
                        }
                    }
                }

                terminal.draw(|f| render::draw(f, &mut state))?;
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

    // ── Config tab shortcuts ──────────────────────────────────────────────────
    if state.active_tab == Tab::Config && state.mode != Mode::AgentRunning && state.mode != Mode::PlanRunning {
        if state.mode == Mode::ProfilePicker {
            match key.code {
                KeyCode::Esc => {
                    state.mode = Mode::Normal;
                    return Ok(true);
                }
                KeyCode::Up => {
                    if state.profile_picker_selected > 0 {
                        state.profile_picker_selected -= 1;
                    }
                    return Ok(true);
                }
                KeyCode::Down => {
                    if !state.profile_picker_entries.is_empty()
                        && state.profile_picker_selected + 1 < state.profile_picker_entries.len()
                    {
                        state.profile_picker_selected += 1;
                    }
                    return Ok(true);
                }
                KeyCode::Enter => {
                    if let Some((name, _)) = state
                        .profile_picker_entries
                        .get(state.profile_picker_selected)
                        .cloned()
                    {
                        // Reload config and switch profile
                        *file = crate::config::ConfigFile::load()?;
                        if file.profiles.contains_key(&name) {
                            *resolved = crate::config::ResolvedConfig::resolve(
                                file,
                                Some(&name),
                                None,
                                None,
                                None,
                            );
                            state.profile = resolved.profile_name.clone();
                            state.model = resolved.model.clone();
                            state.context_tokens = resolved.context_tokens;
                            state.cost_per_mtok_input = resolved.cost_per_mtok_input;
                            state.endpoint = resolved.endpoint.clone();
                            state.hooks_config = if !resolved.active_hook_config.is_empty() {
                                resolved.active_hook_config.clone()
                            } else {
                                resolved.hooks.clone()
                            };
                            state.active_hook_preset = resolved.active_hooks.clone();
                            state.hooks_disabled_profile = resolved.hooks_disabled;
                            state.mcp_server_names = resolved
                                .mcp_servers
                                .iter()
                                .map(|s| s.name.clone())
                                .collect();
                            state.auto_commit = resolved.auto_commit;
                            state.auto_commit_prefix = resolved.auto_commit_prefix.clone();
                            state.git_context_enabled = resolved.git_context;
                            state.push(ConversationEntry::SystemMsg(format!(
                                "✓ switched to {} · {} · {}k ctx",
                                resolved.profile_name,
                                resolved.model,
                                resolved.context_tokens / 1000
                            )));
                        }
                    }
                    state.mode = Mode::Normal;
                    return Ok(true);
                }
                _ => return Ok(true),
            }
        }

        match key.code {
            KeyCode::Char('e') => {
                // Signal the event loop to shell out to $EDITOR
                state.wants_editor = true;
                return Ok(true);
            }
            KeyCode::Char('h') => {
                // Toggle hooks on/off (mirrors /hooks toggle)
                state.hooks_enabled = !state.hooks_enabled;
                let status = if state.hooks_enabled { "on" } else { "off" };
                state.push(ConversationEntry::SystemMsg(
                    format!("⚙ hooks {status}"),
                ));
                return Ok(true);
            }
            KeyCode::Char('p') => {
                // Open profile picker overlay
                let mut entries: Vec<(String, String)> = file
                    .profiles
                    .iter()
                    .map(|(name, p)| (name.clone(), p.model.clone()))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                state.profile_picker_entries = entries;
                state.profile_picker_selected = state
                    .profile_picker_entries
                    .iter()
                    .position(|(n, _)| *n == state.profile)
                    .unwrap_or(0);
                state.mode = Mode::ProfilePicker;
                return Ok(true);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if state.config_scroll > 0 {
                    state.config_scroll -= 1;
                }
                return Ok(true);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.config_scroll += 1;
                return Ok(true);
            }
            // Don't intercept Esc (go to Chat), digits (tab switch), etc. — fall through
            _ => {}
        }
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
                            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                            state.cancel_tx = Some(cancel_tx);
                            state.mode = Mode::PlanRunning;
                            launch_plan(plan, cancel_rx, state, resolved, verbose, dry_run, ui_tx);
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
            KeyCode::Esc => {
                // Esc during slash complete — clear and go back to input_box in Normal
                state.input.clear();
                state.cursor = 0;
                state.mode = Mode::Normal;
            }
            KeyCode::Enter | KeyCode::Tab => {
                let matches = slash_filtered(&state.input);
                if let Some(cmd) = matches.get(state.slash_complete_selected) {
                    let no_arg = matches!(
                        cmd.key,
                        "/quit" | "/exit" | "/q" | "/clear" | "/sessions"
                        | "/new" | "/help" | "/h" | "/ts" | "/list-hooks"
                        | "/profiles" | "/init" | "/stats"
                    );
                    let selected_cmd = cmd.key.to_string();
                    state.input.clear();
                    state.cursor = 0;
                    state.mode = Mode::Normal;
                    if no_arg {
                        // Execute immediately
                        return handle_submit(selected_cmd, state, resolved, file, verbose, dry_run, ui_tx);
                    } else {
                        // Load into input_box so user can type the argument
                        let with_space = format!("{} ", selected_cmd);
                        state.input_box.set_text(&with_space);
                        state.input_box.move_to_end();
                    }
                } else {
                    state.mode = Mode::Normal;
                }
            }
            KeyCode::Backspace => {
                if state.input.len() <= 1 {
                    // Backspaced past `/` — cancel, restore Normal
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
                    // Cancel — clear the picker query, restore Normal
                    state.input.clear();
                    state.cursor = 0;
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
                        // Append #chosen_path into the input_box so it shows with existing text
                        let token = format!("#{} ", chosen);
                        state.input_box.insert_str(&token);
                    }
                    state.input.clear();
                    state.cursor = 0;
                    state.mode = Mode::Normal;
                    state.file_picker = None;
                }
                KeyCode::Backspace => {
                    if fp.query.pop().is_none() {
                        // Backspaced past `#` — cancel picker
                        state.input.clear();
                        state.cursor = 0;
                        state.mode = Mode::Normal;
                        state.file_picker = None;
                    } else {
                        fp.selected = 0;
                        // Sync display string
                        state.input = format!("#{}", fp.query);
                        state.cursor = state.input.len();
                    }
                }
                KeyCode::Char(c) => {
                    fp.query.push(c);
                    fp.selected = 0;
                    // Mirror into state.input for the overlay display
                    state.input = format!("#{}", fp.query);
                    state.cursor = state.input.len();
                }
                _ => {}
            }
        }
        return Ok(true);
    }

    // ── HookWizard mode ───────────────────────────────────────────────────────
    if state.mode == Mode::HookWizard {
        if let Some(wiz) = &mut state.hook_wizard {
            match wiz.step.clone() {
                WizardStep::EnterName => {
                    match key.code {
                        KeyCode::Esc => {
                            state.mode = Mode::Normal;
                            state.hook_wizard = None;
                        }
                        KeyCode::Enter => {
                            let name = wiz.name_input.trim().to_string();
                            if !name.is_empty() {
                                wiz.step = WizardStep::EnterOnEdit;
                            }
                        }
                        KeyCode::Backspace => { wiz.name_input.pop(); }
                        KeyCode::Char(c) => { wiz.name_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::EnterOnEdit => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterName; }
                        KeyCode::Enter => { wiz.step = WizardStep::EnterOnTaskDone; }
                        KeyCode::Backspace => { wiz.on_edit_input.pop(); }
                        KeyCode::Char(c) => { wiz.on_edit_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::EnterOnTaskDone => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterOnEdit; }
                        KeyCode::Enter => { wiz.step = WizardStep::EnterOnPlanStepDone; }
                        KeyCode::Backspace => { wiz.on_task_done_input.pop(); }
                        KeyCode::Char(c) => { wiz.on_task_done_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::EnterOnPlanStepDone => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterOnTaskDone; }
                        KeyCode::Enter => { wiz.step = WizardStep::EnterOnSessionStart; }
                        KeyCode::Backspace => { wiz.on_plan_step_done_input.pop(); }
                        KeyCode::Char(c) => { wiz.on_plan_step_done_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::EnterOnSessionStart => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterOnPlanStepDone; }
                        KeyCode::Enter => { wiz.step = WizardStep::EnterOnSessionEnd; }
                        KeyCode::Backspace => { wiz.on_session_start_input.pop(); }
                        KeyCode::Char(c) => { wiz.on_session_start_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::EnterOnSessionEnd => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterOnSessionStart; }
                        KeyCode::Enter => { wiz.step = WizardStep::Confirm; }
                        KeyCode::Backspace => { wiz.on_session_end_input.pop(); }
                        KeyCode::Char(c) => { wiz.on_session_end_input.push(c); }
                        _ => {}
                    }
                }
                WizardStep::Confirm => {
                    match key.code {
                        KeyCode::Esc => { wiz.step = WizardStep::EnterOnSessionEnd; }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            state.mode = Mode::Normal;
                            state.hook_wizard = None;
                        }
                        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                            let name = wiz.name_input.trim().to_string();
                            let parse_cmds = |s: &str| -> Vec<String> {
                                s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
                            };
                            let on_edit              = parse_cmds(&wiz.on_edit_input);
                            let on_task_done         = parse_cmds(&wiz.on_task_done_input);
                            let on_plan_step_done    = parse_cmds(&wiz.on_plan_step_done_input);
                            let on_session_start     = parse_cmds(&wiz.on_session_start_input);
                            let on_session_end       = parse_cmds(&wiz.on_session_end_input);
                            let hook_cfg = crate::hooks::HookConfig {
                                on_edit,
                                on_task_done,
                                on_plan_step_done,
                                on_session_start,
                                on_session_end,
                            };
                            // Write [hooks.NAME] section to config
                            crate::hooks::write_config_hooks(&name, &hook_cfg);
                            // Persist active_hooks = name
                            crate::hooks::write_active_hooks(Some(&name));
                            // Apply to live state
                            state.hooks_config = hook_cfg.clone();
                            state.active_hook_preset = Some(name.clone());
                            if !state.available_hook_presets.contains(&name) {
                                state.available_hook_presets.push(name.clone());
                                state.available_hook_presets.sort();
                            }
                            state.hooks_enabled = true;
                            state.mode = Mode::Normal;
                            state.hook_wizard = None;
                            state.push(ConversationEntry::SystemMsg(
                                format!("⚙ hooks.{name} saved and active"),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        return Ok(true);
    }

    // ── PlanRunning — Esc cancels ─────────────────────────────────────────────
    if state.mode == Mode::PlanRunning && key.code == KeyCode::Esc {
        if let Some(tx) = state.cancel_tx.take() {
            let _ = tx.send(());
        }
        state.mode = Mode::Normal;
        state.push(ConversationEntry::SystemMsg("plan cancelled".to_string()));
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
        // Ctrl+C — quit (only when idle)
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if !matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning | Mode::AskingUser) {
                return Ok(false);
            }
        }
        // Esc — cancel running agent/plan, or dismiss overlays
        (KeyModifiers::NONE, KeyCode::Esc) if matches!(state.mode, Mode::AgentRunning | Mode::AskingUser) => {
            if state.mode == Mode::AskingUser {
                if let Some(tx) = state.pending_ask_reply.take() {
                    let _ = tx.send("[user cancelled the question — proceed with your best judgement]".to_string());
                }
                state.push(ConversationEntry::AskReply("(cancelled)".to_string()));
                state.input_box.clear();
                state.mode = Mode::AgentRunning;
            } else {
                // Cancel agent
                if let Some(tx) = state.cancel_tx.take() {
                    let _ = tx.send(());
                }
            }
            return Ok(true);
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
                && state.input_box.is_empty()
                && state.mode == Mode::Normal
                && state.attached_files.is_empty() => {
            state.sidebar_focused = true;
        }
        // 1-5 — switch tabs (only when input is empty and not running)
        (KeyModifiers::NONE, KeyCode::Char('1')) if state.input_box.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Chat;
        }
        (KeyModifiers::NONE, KeyCode::Char('2')) if state.input_box.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Config;
        }
        (KeyModifiers::NONE, KeyCode::Char('3')) if state.input_box.is_empty()
            && state.mode == Mode::Normal => {
            state.active_tab = Tab::Stats;
        }
        (KeyModifiers::NONE, KeyCode::Char('4')) if state.input_box.is_empty()
            && state.mode == Mode::Normal
            && state.plan_ever_active => {
            state.active_tab = Tab::Plan;
        }
        (KeyModifiers::NONE, KeyCode::Char('5')) if state.input_box.is_empty()
            && state.mode == Mode::Normal
            && state.git_available => {
            state.active_tab = Tab::Git;
            git_view::load_git_tab(state);
        }
        // 'u' in Git tab opens the checkpoint picker
        (KeyModifiers::NONE, KeyCode::Char('u')) if state.input_box.is_empty()
            && state.mode == Mode::Normal
            && state.active_tab == Tab::Git => {
            let _ = execute_command("/undo", state, resolved, file)?;
        }
        // 'd' opens the full-diff overlay (only in Normal mode with git available)
        (KeyModifiers::NONE, KeyCode::Char('d')) if state.input_box.is_empty()
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
        // Scroll up/down — only when in Normal/AgentRunning and textarea not focused
        (KeyModifiers::NONE, KeyCode::Up) if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_add(5);
            } else {
                state.scroll = state.scroll.saturating_add(5);
            }
        }
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_add(20);
            } else {
                state.scroll = state.scroll.saturating_add(20);
            }
        }
        (KeyModifiers::NONE, KeyCode::Down) if matches!(state.mode, Mode::AgentRunning | Mode::PlanRunning) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_sub(5);
            } else {
                state.scroll = state.scroll.saturating_sub(5);
            }
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            if state.active_tab == Tab::Stats {
                state.stats_scroll = state.stats_scroll.saturating_sub(20);
            } else {
                state.scroll = state.scroll.saturating_sub(20);
            }
        }
        // Backspace — remove focused chip (if any) before delegating to input_box
        (KeyModifiers::NONE, KeyCode::Backspace) if state.mode == Mode::Normal => {
            if let Some(idx) = state.focused_chip {
                if idx < state.attached_files.len() {
                    state.attached_files.remove(idx);
                }
                state.focused_chip = if state.attached_files.is_empty() {
                    None
                } else {
                    Some(idx.min(state.attached_files.len() - 1))
                };
                return Ok(true);
            }
            // Fall through to input_box handler below
            state.focused_chip = None;
            let action = state.input_box.handle_key(key);
            if action == InputAction::Submit {
                let text = state.input_box.get_text();
                let input = text.trim().to_string();
                if !input.is_empty() {
                    return handle_submit(input, state, resolved, file, verbose, dry_run, ui_tx);
                }
            }
        }
        // Shift+Enter / Ctrl+Enter / Alt+Enter — insert newline in Normal/AskingUser
        // Note: many terminals collapse Shift/Ctrl+Enter to plain Enter; Alt+Enter is reliable.
        (m, KeyCode::Enter)
            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT)
                && matches!(state.mode, Mode::Normal | Mode::AskingUser) =>
        {
            state.input_box.insert_newline();
        }
        // All other keys in Normal/AskingUser — delegate to input_box
        _ if matches!(state.mode, Mode::Normal | Mode::AskingUser) => {
            state.focused_chip = None;
            let action = state.input_box.handle_key(key);
            match action {
                InputAction::Passthrough => {
                    // Not handled by input_box — ignore (special keys already caught above)
                }
                InputAction::Submit => {
                    let text = state.input_box.get_text();
                    let input = text.trim().to_string();
                    if !input.is_empty() {
                        state.input_box.clear();
                        return handle_submit(input, state, resolved, file, verbose, dry_run, ui_tx);
                    }
                }
                InputAction::Handled => {
                    // After each keystroke in Normal mode, check for trigger characters
                    if state.mode == Mode::Normal {
                        let text = state.input_box.get_text();

                        // `#` trigger: last char on current line is `#` and it's on its own word
                        // (word boundary: start of text or preceded by whitespace)
                        let last_line = text.split('\n').last().unwrap_or("");
                        let trimmed = last_line.trim_start();
                        if trimmed == "#" || (last_line.ends_with('#') && last_line[..last_line.len()-1].ends_with(|c: char| c.is_whitespace())) {
                            // Transfer text before `#` back into input_box, open picker
                            let prefix = text[..text.rfind('#').unwrap_or(0)].to_string();
                            state.input_box.clear();
                            if !prefix.trim().is_empty() {
                                state.input_box.set_text(&prefix);
                            }
                            state.input = String::new();
                            state.cursor = 0;
                            state.file_picker = Some(FilePickerState {
                                all_files: gather_files(),
                                query: String::new(),
                                selected: 0,
                            });
                            state.mode = Mode::FilePicker;
                        }
                        // `/` at start of a single-line input: switch to slash complete
                        else if text == "/" {
                            state.input_box.clear();
                            state.input = "/".to_string();
                            state.cursor = 1;
                            state.slash_complete_selected = 0;
                            state.mode = Mode::SlashComplete;
                        }
                    }
                }
            }
        }
        _ => {}
    }

    Ok(true)
}

// ── Command registry ───────────────────────────────────────────────────────────

/// Represents a parsed command with its argument (if any).
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Plan(String),
    Quick(String),
    Unknown(String),
}

impl Command {
    /// Parse input into a command. Returns None if input doesn't start with `/`.
    fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        // Split on first whitespace to separate command from argument
        let (cmd, arg) = input[1..].split_once(' ').unwrap_or((&input[1..], ""));
        let arg = arg.trim();

        match cmd {
            "plan" if !arg.is_empty() => Some(Command::Plan(arg.to_string())),
            "plan" => Some(Command::Plan(String::new())), // for usage error
            "quick" if !arg.is_empty() => Some(Command::Quick(arg.to_string())),
            "quick" => Some(Command::Quick(String::new())),
            _ => Some(Command::Unknown(cmd.to_string())),
        }
    }
}

// ── Submit helper ─────────────────────────────────────────────────────────────

fn handle_submit(
    input: String,
    state: &mut AppState,
    resolved: &mut ResolvedConfig,
    file: &mut ConfigFile,
    verbose: bool,
    dry_run: bool,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<bool> {
    if state.mode == Mode::AskingUser {
        state.push(ConversationEntry::AskReply(input.clone()));
        if let Some(tx) = state.pending_ask_reply.take() {
            let _ = tx.send(input);
        }
        state.mode = Mode::AgentRunning;
        return Ok(true);
    }

    // Parse #path tokens and attach referenced files
    {
        let re_tokens: Vec<&str> = input.split_whitespace()
            .filter(|t| t.starts_with('#') && t.len() > 1)
            .collect();
        for token in re_tokens {
            let path = &token[1..];
            if !state.attached_files.iter().any(|f| f.path == path) {
                if let Ok(content) = std::fs::read_to_string(path) {
                    state.attached_files.push(AttachedFile { path: path.to_string(), content });
                }
            }
        }
    }

    // Route input through command parser
    let cmd = Command::parse(&input);

    // Handle /plan and /quick specially (they need ui_tx and other params)
    if let Some(Command::Plan(task)) = cmd {
        if task.is_empty() {
            state.push(ConversationEntry::SystemMsg("usage: /plan \"describe the task\"".to_string()));
        } else {
            generate_and_show_plan(task, state, resolved, ui_tx.clone());
        }
        return Ok(true);
    }
    if let Some(Command::Quick(task)) = cmd {
        if task.is_empty() {
            state.push(ConversationEntry::SystemMsg("usage: /quick \"task\"".to_string()));
        } else {
            launch_quick(task, state, resolved, verbose, dry_run, ui_tx)?;
        }
        return Ok(true);
    }

    // All other slash commands (including unknown ones)
    if input.starts_with('/') {
        let keep = execute_command(&input, state, resolved, file)?;
        if !keep {
            return Ok(false);
        }
    } else {
        launch_agent(input, state, resolved, verbose, dry_run, ui_tx)?;
    }
    Ok(true)
}

// ── Config tab inline edit apply ──────────────────────────────────────────────

/// Apply a config field edit: update AppState, ResolvedConfig, ConfigFile, and save to disk.

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
                "Commands: /plan \"task\"  /quick \"task\"  /init  /cd  /profile  /profiles  /ts  /hooks [on|off|list|<preset>]  /list-hooks  /pie  /undo [n]  /diff  /clear  /sessions  /resume [n]  /rollback [n]  /new  /quit\nCtrl+H  session history  ·  Ctrl+P  command palette  ·  d  open diff overlay\nIn plan review: ↑↓ navigate  e annotate  d clear note  a approve & run  Esc cancel\nIn git repo: press 5 for Git tab · /undo to revert · /diff to review changes".to_string(),
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
                "disable" | "off" => {
                    state.hooks_enabled = false;
                    state.push(ConversationEntry::SystemMsg("⚙ hooks disabled".to_string()));
                }
                "enable" | "on" => {
                    state.hooks_enabled = true;
                    let label = state.active_hook_preset.as_deref().unwrap_or("on");
                    state.push(ConversationEntry::SystemMsg(format!("⚙ hooks enabled  ({label})")));
                }
                "list" => {
                    if state.available_hook_presets.is_empty() {
                        state.push(ConversationEntry::SystemMsg(
                            "no hook configs found — add [hooks.NAME] sections to config.toml, or run /hooks setup".to_string(),
                        ));
                    } else {
                        let active = state.active_hook_preset.as_deref().unwrap_or("none");
                        let list = state.available_hook_presets.join(", ");
                        state.push(ConversationEntry::SystemMsg(
                            format!("hooks: {list}\nactive: {active}\n/hooks <name> to switch"),
                        ));
                    }
                }
                "setup" => {
                    state.hook_wizard = Some(HookWizardState {
                        step: WizardStep::EnterName,
                        name_input: String::new(),
                        on_edit_input: String::new(),
                        on_task_done_input: String::new(),
                        on_plan_step_done_input: String::new(),
                        on_session_start_input: String::new(),
                        on_session_end_input: String::new(),
                    });
                    state.mode = Mode::HookWizard;
                }
                "" => {
                    let status = if state.hooks_enabled { "enabled" } else { "disabled" };
                    let active = state.active_hook_preset.as_deref().unwrap_or("none");
                    state.push(ConversationEntry::SystemMsg(
                        format!("⚙ hooks {status}  active: {active}\nusage: /hooks <name> | enable | disable | list | setup"),
                    ));
                }
                hook_name => {
                    // Switch to a named hook config from [hooks.NAME]
                    match ConfigFile::load() {
                        Ok(cfg) => {
                            if let Some(hook_cfg) = cfg.hooks.get(hook_name) {
                                state.hooks_config = hook_cfg.clone();
                                state.active_hook_preset = Some(hook_name.to_string());
                                state.hooks_enabled = true;
                                // Persist the active selection
                                crate::hooks::write_active_hooks(Some(hook_name));
                                state.push(ConversationEntry::SystemMsg(
                                    format!("⚙ hooks.{hook_name} active"),
                                ));
                            } else {
                                let available = if state.available_hook_presets.is_empty() {
                                    "none configured".to_string()
                                } else {
                                    state.available_hook_presets.join(", ")
                                };
                                state.push(ConversationEntry::SystemMsg(
                                    format!("hooks.{hook_name} not found  available: {available}"),
                                ));
                            }
                        }
                        Err(e) => {
                            state.push(ConversationEntry::SystemMsg(
                                format!("could not load config: {e}"),
                            ));
                        }
                    }
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
        "/pie" => {
            let mut lines: Vec<String> = Vec::new();

            // Graph health
            match &state.project_graph {
                Some(g) => {
                    let mins_ago = (chrono::Utc::now().timestamp() - g.last_indexed).max(0) / 60;
                    let age = if mins_ago < 1 { "just now".to_string() }
                              else if mins_ago < 60 { format!("{mins_ago}m ago") }
                              else { format!("{}h ago", mins_ago / 60) };
                    lines.push(format!(
                        "◈ Project graph  {} clusters · {} files · indexed {}",
                        g.clusters.len(),
                        g.file_lines.len(),
                        age,
                    ));
                }
                None => lines.push("◈ Project graph  not loaded".to_string()),
            }

            // Narrative
            match &state.project_narrative {
                Some(n) if !n.architecture_summary.is_empty() => {
                    let age = if n.last_synthesized == 0 {
                        "unknown".to_string()
                    } else {
                        let secs = (chrono::Utc::now().timestamp() - n.last_synthesized).max(0);
                        if secs < 120 { "just now".to_string() }
                        else if secs < 3600 { format!("{}m ago", secs / 60) }
                        else if secs < 86_400 { format!("{}h ago", secs / 3600) }
                        else { format!("{}d ago", secs / 86_400) }
                    };
                    lines.push(format!(
                        "◈ Narrative  {} cluster summaries · synthesized {}",
                        n.cluster_summaries.len(),
                        age,
                    ));
                    lines.push(format!("  Architecture: {}", &n.architecture_summary.chars().take(80).collect::<String>()));
                }
                _ => lines.push("◈ Narrative  not generated (cold startup didn't complete)".to_string()),
            }

            // Task memory
            let recent = crate::task_memory::load_recent(200);
            if recent.is_empty() {
                lines.push("◈ Task memory  0 records (complete a task to build history)".to_string());
            } else {
                lines.push(format!("◈ Task memory  {} records", recent.len()));
                for rec in recent.iter().take(3) {
                    lines.push(format!("  [{}] {}", rec.age_str(), &rec.summary.chars().take(70).collect::<String>()));
                }
            }

            // Context weights
            let weight_count = state.context_weights.file_weights.len();
            if weight_count > 0 {
                lines.push(format!("◈ Context weights  {} files tracked", weight_count));
            } else {
                lines.push("◈ Context weights  no data yet".to_string());
            }

            state.push(ConversationEntry::SystemMsg(lines.join("\n")));
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

fn resolve_hooks_with_state(
    _resolved: &ResolvedConfig,
    state: &AppState,
) -> crate::hooks::HookConfig {
    if !state.hooks_enabled {
        return crate::hooks::HookConfig::default();
    }
    // hooks_config is kept in sync with the active [hooks.NAME] selection
    state.hooks_config.clone()
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
    let resolved_hooks = resolve_hooks_with_state(resolved, state);
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
        project_context: match (&state.project_narrative, &state.project_graph) {
            (Some(n), Some(g)) if !n.architecture_summary.is_empty() => {
                let candidate_files: Vec<String> = state.attached_files.iter().map(|f| f.path.clone()).collect();
                let recent = crate::task_memory::find_relevant(&candidate_files, 3);
                Some(n.to_context_package(g, &[], 8, &recent))
            }
            (_, Some(g)) => g.to_prompt_section(8),
            _ => None,
        },
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
        project_context: match (&state.project_narrative, &state.project_graph) {
            (Some(n), Some(g)) if !n.architecture_summary.is_empty() => {
                let candidate_files: Vec<String> = state.attached_files.iter().map(|f| f.path.clone()).collect();
                let recent = crate::task_memory::find_relevant(&candidate_files, 3);
                Some(n.to_context_package(g, &[], 8, &recent))
            }
            (_, Some(g)) => g.to_prompt_section(8),
            _ => None,
        },
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

    // Use the graph already loaded at startup; fall back to a fresh build if missing.
    let graph = state.project_graph.clone().unwrap_or_else(|| {
        crate::pie::ProjectGraph::build_fresh(std::path::Path::new("."), 500)
    });
    let narrative = state.project_narrative.clone();

    tokio::spawn(async move {
        let tx_plan = ui_tx.clone();
        let on_chunk = move |chunk: &str| {
            // Forward thinking chunks so the TUI shows planning progress
            let _ = tx_plan.send(UiEvent::ThinkingChunk(chunk.to_string()));
        };
        match plan::generate_plan(&task, &client, &project, &context_files, &graph, narrative.as_ref(), on_chunk).await {
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
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
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
    let resolved_hooks = resolve_hooks_with_state(resolved, state);
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
        project_context: None, // executor steps have explicit file context — no project map needed
    };

    tokio::spawn(async move {
        // Use a shared cancellation flag so we can check it between steps
        // without consuming the oneshot receiver more than once.
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let ui_tx_cancel = ui_tx.clone();
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            cancelled_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = ui_tx_cancel.send(UiEvent::AgentError("cancelled".to_string()));
        });

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
            // Check cancellation before starting each step
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }

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
/// Extract file paths from tool actions that write/modify files.
/// `collecting_tools` entries look like "edit_file(src/foo.rs)" or "read_file(src/bar.rs)".
/// Only write-class tools count as modified.
fn extract_modified_files(tool_actions: &[String]) -> Vec<String> {
    const WRITE_TOOLS: &[&str] = &["edit_file", "write_file", "patch_file", "create_file"];
    let mut files = Vec::new();
    for action in tool_actions {
        if let Some(paren) = action.find('(') {
            let tool_name = &action[..paren];
            if WRITE_TOOLS.contains(&tool_name) {
                let path = action[paren + 1..].trim_end_matches(')');
                if !path.is_empty() && !files.contains(&path.to_string()) {
                    files.push(path.to_string());
                }
            }
        }
    }
    files
}

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

