use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::budget::{Budget, LoopDetector};
use crate::cache::FileCache;
use crate::client::{Client, ContentPart, Message, MessageContent, Tool, ToolCall};
use crate::history::History;
use crate::hooks::{self, HookConfig};
use crate::mcp::McpClient;
use crate::tools::{self};
use crate::tui::UiEvent;

const MAX_TOOL_CALLS: usize = 40;

const SYSTEM_PROMPT_BASE: &str = r#"You are PareCode, a focused coding assistant. Complete tasks using the available tools.

# Principles
- **Minimum tool calls.** Every unnecessary read, search, or bash costs tokens and time.
- **Act, don't explore.** If you know what to change, change it. Do not read to confirm what you already know.
- **Ask only when blocked.** Use `ask_user` if the task names a file or symbol that does not appear in the project graph and you cannot proceed. Never ask about things you can look up.
- **When done, stop.** Do not summarise what you did unless asked.

# Project graph
When a project graph appears below, **it is the authoritative index of this codebase** — every cluster, file, and symbol with exact line numbers, pre-computed and injected for you.

**You must use it.** Do not replicate its work with `search` or `bash` — that wastes tokens and produces no new information.
- Symbol location → graph has the exact file and line. Go straight to `read_file(path, line_range=[N, M])`.
- File structure → graph has every function/struct name. Do not read the file first to "see what's there".
- **`search` is for call-site discovery and cross-file pattern matching — not for locating things already in the graph.**
- Read only to obtain edit hashes for the lines you are about to change.

If no project graph appears, use `list_files` once to orient yourself, then proceed.

# Reading files
**Small files (≤300 lines):** `read_file(path)` returns the full file with line numbers and hashes. One call is enough — edit immediately.

**Large files (>300 lines):** `read_file(path)` returns preamble + symbol index + tail. The symbol index gives exact line numbers for every function and struct. Then: `read_file(path, line_range=[start, end])` to fetch only the section you need.

**Do not re-read a file unless you have edited it since your last read.** Before an edit the cache returns identical content — a re-read is a wasted call. After an edit you must re-read the changed region to get fresh hashes.

# Editing — the hash lifecycle
Each line is prefixed `N [hash] | content`. The hash is the required anchor for `edit_file`.

1. Read the section → get hashes.
2. `edit_file` — `old_str` must be copied verbatim (including the `N [hash] |` prefix) from that read.
3. The edit result returns a fresh window with **new hashes** for the changed region.
4. Further edits to the same region: use hashes from step 3. Hashes from step 1 are stale.
5. Edit to a different region of the same file: `read_file(path, line_range=[...])` for that location first.

**Only edit one file per turn.** For 2+ non-adjacent changes in one file, use `patch_file` in a single call — never chain multiple `edit_file` calls.

# Choosing the right mutation tool
- `edit_file` — single contiguous change, insert, or append.
- `patch_file` — 2+ non-adjacent locations in one file. Always prefer over multiple `edit_file` calls.
- `write_file` — new files only. Never use on an existing file.

# bash — builds and state changes only
`bash` is for: compiling, running tests, git, package managers, and anything that changes external state.

**Never use bash to read files or explore the project.** These are forbidden via bash:
- `cat`, `head`, `tail` → use `read_file` (provides hashes; cached; stale eviction aware)
- `grep`, `rg` → use `search` (structured results, no wasted context)
- `ls`, `find` → use `list_files` (respects project ignore rules)

Using bash for file reading bypasses the session cache (re-hits disk every call), produces output without hashes (breaking `edit_file`), and fills context with unstructured text that cannot be evicted after edits.

# search
- YES: finding all call sites of a function, checking a pattern exists across files, confirming a string was fully removed.
- NO: locating a function or symbol — the project graph has the exact line number. NO: verifying an edit — the edit result already shows the updated lines."#;

/// Quick mode — single API call, no multi-turn loop, minimal context.
/// Targets < 2k tokens total. No file loading, no session history.
/// Allows at most 1 tool call before returning (edit_file, search, bash read-only).
pub async fn run_quick(
    task: &str,
    client: &Client,
    config: &AgentConfig,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    let task_start = std::time::Instant::now();
    let task_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string());
    const QUICK_SYSTEM: &str = "You are PareCode in quick mode. Answer concisely in one response. \
If a tool call is needed, make exactly one — prefer edit_file or search. \
Do not read files unless strictly necessary. Keep responses short.";

    // Lean tool list — only the tools that make sense for quick tasks
    let quick_tools: Vec<crate::client::Tool> = tools::all_definitions()
        .into_iter()
        .filter(|t| matches!(t.name.as_str(), "edit_file" | "search" | "read_file" | "bash"))
        .collect();

    let messages = vec![Message {
        role: "user".to_string(),
        content: MessageContent::from(task.to_string()),
        tool_calls: vec![],
    }];

    let tx_clone = ui_tx.clone();
    let response = client
        .chat(QUICK_SYSTEM, &messages, &quick_tools, move |chunk| {
            let _ = tx_clone.send(UiEvent::Chunk(chunk.to_string()));
        })
        .await?;

    let total_input = response.input_tokens;
    let total_output = response.output_tokens;

    // Send token stats so TUI tracks inflight usage (survives cancel/crash)
    let _ = ui_tx.send(UiEvent::TokenStats {
        _input: total_input,
        _output: total_output,
        total_input,
        total_output,
        tool_calls: 0,
    }); 

    // Execute at most one tool call
    if let Some(tc) = response.tool_calls.first() {
        let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
        let kind = ToolKind::classify(&tc.name, &args);
        let mut cache = FileCache::default();
        let history = History::default();
        let _ = ui_tx.send(UiEvent::ToolCall {
            name: tc.name.clone(),
            args_summary: format_args_summary(&args),
        });
        let raw = dispatch_tool(&tc.name, &args, &kind, &mut cache, &history, &ui_tx, &config.mcp).await;
        let _ = ui_tx.send(UiEvent::ToolResult {
            summary: raw.lines().take(30).collect::<Vec<_>>().join("\n"),
        });
    }

    let _ = ui_tx.send(UiEvent::AgentDone {
        input_tokens: total_input,
        output_tokens: total_output,
        tool_calls: response.tool_calls.len().min(1),
        compressed_count: 0,
        duration_secs: task_start.elapsed().as_secs() as u32,
        cwd: task_cwd,
    });

    Ok(())
}


/// Run agent, emitting UiEvents to a ratatui TUI over `ui_tx`.
/// `attached` is a list of (path, content) pairs pre-loaded by the user via @.
/// `prior_context` is an optional preamble summarising earlier turns in this session.
pub async fn run_tui(
    task: &str,
    client: &Client,
    config: &AgentConfig,
    attached: Vec<(String, String)>,
    prior_context: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    shared_cache: std::sync::Arc<tokio::sync::Mutex<crate::cache::FileCache>>,
) -> Result<()> {
    let task_start = std::time::Instant::now();
    let task_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string());
    // MCP tools are appended alongside native tools each turn
    let mcp_tools = config.mcp.all_tools().await;
    let mcp_tool_defs: Vec<Tool> = mcp_tools.iter().map(|mt| Tool {
        name: mt.qualified_name.clone(),
        description: mt.description.clone(),
        parameters: mt.input_schema.clone(),
    }).collect();

    let mut messages: Vec<Message> = Vec::new();
    let mut total_input_tokens = 0u32;
    let mut total_output_tokens = 0u32;
    let mut tool_call_count = 0usize;
    let mut turn: usize = 0;

    let budget = Budget::new(config.context_tokens);
    let mut history = History::default();
    let mut cache = shared_cache.lock().await;
    cache.next_turn(); // advance turn counter at task boundary
    let mut loop_detector = LoopDetector::default();

    // Fetch git status once so build_system_prompt stays pure.
    let git_status: Option<String> = if config.git_context {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::git::GitRepo::open(&cwd))
            .and_then(|repo| repo.status_short().ok())
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    };
    let system_prompt = build_system_prompt(config, git_status.as_deref());
    let system_prompt = system_prompt.as_str();
    let system_tokens = crate::budget::estimate_tokens(system_prompt);

    let user_content = build_user_message(task, prior_context.as_deref(), &attached);

    // ── Git checkpoint ────────────────────────────────────────────────────────
    // Create a checkpoint before the task starts. If the tree is dirty, this
    // commits all pending changes as a WIP checkpoint so /undo can restore them.
    // Skips silently if not in a git repo or git_context is disabled.
    let checkpoint_hash: Option<String> = if config.git_context {
        std::env::current_dir().ok().and_then(|cwd| {
            crate::git::GitRepo::open(&cwd).and_then(|repo| {
                let summary: String = task
                    .lines()
                    .next()
                    .unwrap_or(task)
                    .chars()
                    .take(60)
                    .collect();
                repo.checkpoint(&summary).ok()
            })
        })
    } else {
        None
    };

    messages.push(Message {
        role: "user".to_string(),
        content: MessageContent::from(user_content),
        tool_calls: vec![],
    });

    loop {
        cache.next_turn();
        turn += 1;

        // ── Hard tool-call budget ─────────────────────────────────────────────
        if tool_call_count >= MAX_TOOL_CALLS {
            let _ = ui_tx.send(UiEvent::ToolBudgetHit { limit: MAX_TOOL_CALLS });
            break;
        }

        // ── Proactive token budget enforcement ────────────────────────────────
        let (est, compressed) = budget.enforce(&mut messages, system_tokens);
        if compressed {
            let _ = ui_tx.send(UiEvent::BudgetWarning);
        }
        let _ = ui_tx.send(UiEvent::ContextUpdate {
            used: est,
            total: budget.total_context(),
            compressed,
        });

        // ── Phase-adaptive tool selection ────────────────────────────────────
        // Only send tools relevant to the current phase of work.
        // Saves ~400-800 tokens/turn compared to sending all 9 tools every time.
        let mut tools = tools::tools_for_turn(turn, history.compressed_count() > 0);
        tools.extend(mcp_tool_defs.iter().cloned());

        // ── Call the model ────────────────────────────────────────────────────
        // ThinkParser splits <think>…</think> blocks from normal text so the TUI
        // can render reasoning separately. It owns all buffering state — no Arc/Mutex needed.
        let tx_clone = ui_tx.clone();
        let parser = std::sync::Arc::new(std::sync::Mutex::new(ThinkParser::new()));
        let parser_c = parser.clone();
        let response = client
            .chat(system_prompt, &messages, &tools, move |chunk| {
                let (normal, thinking) = parser_c.lock().unwrap().push(chunk);
                if !normal.is_empty() { let _ = tx_clone.send(UiEvent::Chunk(normal)); }
                if !thinking.is_empty() { let _ = tx_clone.send(UiEvent::ThinkingChunk(thinking)); }
            })
            .await?;
        // Drain any remainder held back by the lookahead buffer.
        {
            let p = std::mem::replace(&mut *parser.lock().unwrap(), ThinkParser::new());
            let (normal, thinking) = p.finish();
            if !normal.is_empty() { let _ = ui_tx.send(UiEvent::Chunk(normal)); }
            if !thinking.is_empty() { let _ = ui_tx.send(UiEvent::ThinkingChunk(thinking)); }
        }

        total_input_tokens += response.input_tokens;
        total_output_tokens += response.output_tokens;

        // Always send token stats so the TUI/telemetry can track usage live.
        // If the agent crashes or is cancelled, partial stats are already recorded.
        if response.input_tokens > 0 || response.output_tokens > 0 {
            let _ = ui_tx.send(UiEvent::TokenStats {
                _input: response.input_tokens,
                _output: response.output_tokens,
                total_input: total_input_tokens,
                total_output: total_output_tokens,
                tool_calls: tool_call_count,
            });
        }

        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::from(response.text.clone()),
            tool_calls: response.tool_calls.clone(),
        });

        // No tool calls → done, unless the model emitted XML <invoke> syntax
        // instead of JSON tool calls (a known failure mode for some models trained
        // on Anthropic-style prompts). Detect and send a corrective message.
        if response.tool_calls.is_empty() {
            if response.text.contains("<invoke") && response.text.contains("</invoke>") {
                messages.push(Message {
                    role: "user".to_string(),
                    content: MessageContent::from(
                        "You used XML <invoke> tool syntax instead of JSON tool calls. \
                         Use the JSON tool_calls format provided in the API — do not emit \
                         XML. Retry your intended action now using the correct format."
                            .to_string(),
                    ),
                    tool_calls: vec![],
                });
                continue;
            }
            break;
        }

        // ── Execute tool calls ────────────────────────────────────────────────
        // All tool calls from a single response are executed and all results
        // returned together (required by the OpenAI API spec).
        let mut tool_results: Vec<ContentPart> = Vec::new();
        let mut mutated_files: std::collections::HashSet<String> = std::collections::HashSet::new();

        for tc in &response.tool_calls {
            let (part, dispatched) = execute_one_tool_call(
                tc,
                &mut mutated_files,
                &mut messages,
                &mut cache,
                &mut history,
                &mut loop_detector,
                config,
                &ui_tx,
            ).await;
            if dispatched {
                tool_call_count += 1;
            }
            if let Some(part) = part {
                tool_results.push(part);
            }
        }

        messages.push(Message {
            role: "tool".to_string(),
            content: MessageContent::Parts(tool_results),
            tool_calls: vec![],
        });

        // Strip CoT reasoning from the assistant turn that triggered these tool
        // calls — it wastes context once results are in. Final assistant turns
        // (no tool calls) are never touched — they're the visible response.
        strip_cot_from_last_assistant(&mut messages);

        // Update inflight tool count immediately after execution so the TUI
        // shows the correct count while the next API call streams.
        let _ = ui_tx.send(UiEvent::TokenStats {
            _input: 0,
            _output: 0,
            total_input: total_input_tokens,
            total_output: total_output_tokens,
            tool_calls: tool_call_count,
        });
    }

    // ── on_task_done hooks ────────────────────────────────────────────────────
    // Run after the agent loop. Output goes to TUI only — not into context.
    for hr in hooks::run_task_done_hooks(&config.hooks, config.hooks_enabled).await {
        let _ = ui_tx.send(UiEvent::HookOutput {
            event: "on_task_done".to_string(),
            output: hr.output,
            exit_code: hr.exit_code,
        });
    }

    // ── Git post-task ─────────────────────────────────────────────────────────
    // Emit a diff notification and optionally auto-commit.
    if config.git_context {
        if let Some(cwd) = std::env::current_dir().ok() {
            if let Some(repo) = crate::git::GitRepo::open(&cwd) {
                let ref_pt = checkpoint_hash.as_deref().unwrap_or("HEAD");
                let pt = repo.post_task(ref_pt, task, config.auto_commit, &config.auto_commit_prefix);
                if let Some(stat) = pt.diff_stat {
                    let _ = ui_tx.send(UiEvent::GitChanges {
                        stat,
                        checkpoint_hash: checkpoint_hash.clone(),
                        files_changed: pt.files_changed,
                    });
                }
                if let Some(msg) = pt.auto_committed {
                    let _ = ui_tx.send(UiEvent::GitAutoCommit { message: msg });
                }
                if let Some(err) = pt.commit_error {
                    let _ = ui_tx.send(UiEvent::GitError(err));
                }
            }
        }
    }

    // ── Task complete ─────────────────────────────────────────────────────────
    // Send a final context update so the status bar reflects post-task size
    let final_est = crate::budget::estimate_messages(&messages) + system_tokens;
    let _ = ui_tx.send(UiEvent::ContextUpdate {
        used: final_est,
        total: budget.total_context(),
        compressed: false,
    });
    let _ = ui_tx.send(UiEvent::AgentDone {
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        tool_calls: tool_call_count,
        compressed_count: history.compressed_count(),
        duration_secs: task_start.elapsed().as_secs() as u32,
        cwd: task_cwd,
    });

    Ok(())
}


/// Load project conventions from AGENTS.md, CLAUDE.md, or .parecode/conventions.md.
/// Returns None if no conventions file is found.
fn load_conventions() -> Option<String> {
    let candidates = [
        "AGENTS.md",
        "CLAUDE.md",
        ".parecode/conventions.md",
    ];
    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return Some(format!("\n\n# Project conventions ({path})\n\n{trimmed}"));
            }
        }
    }
    None
}

pub struct AgentConfig {
    pub verbose: bool,
    pub dry_run: bool,
    pub context_tokens: u32,
    pub _profile_name: String,
    pub _model: String,
    pub _show_timestamps: bool,
    pub mcp: Arc<McpClient>,
    /// Resolved hook commands (may be from explicit config or auto-detected).
    pub hooks: Arc<HookConfig>,
    /// When false, all hooks are suppressed for this run (set by `/hooks off`).
    pub hooks_enabled: bool,
    /// Auto-commit all changes after successful task completion.
    pub auto_commit: bool,
    /// Prefix for auto-commit messages (e.g. "parecode: ").
    pub auto_commit_prefix: String,
    /// Enable git integration: checkpoint before task, git status in system prompt, diff after.
    pub git_context: bool,
    /// Pre-rendered project context (narrative + graph) for injection into the system prompt.
    /// When None, no project structure is injected (executor plan steps).
    pub project_context: Option<String>,
}

// ── Pure prompt-assembly helpers ──────────────────────────────────────────────

/// Assemble the full system prompt from base + optional project context + conventions + git status.
/// `git_status` is pre-fetched by the caller so this function stays pure and testable.
pub fn build_system_prompt(config: &AgentConfig, git_status: Option<&str>) -> String {
    let mut prompt = SYSTEM_PROMPT_BASE.to_string();

    if let Some(ref ctx) = config.project_context {
        prompt.push_str("\n\n");
        prompt.push_str(ctx);
    }

    if let Some(conventions) = load_conventions() {
        prompt.push_str(&conventions);
    }

    if let Some(status) = git_status {
        if !status.trim().is_empty() {
            prompt.push_str(&format!("\n\n# Git status\n\n```\n{}\n```", status.trim()));
        }
    }

    prompt
}

/// Assemble the first user message: prior context + attached files + task.
pub fn build_user_message(
    task: &str,
    prior_context: Option<&str>,
    attached: &[(String, String)],
) -> String {
    let mut s = String::new();
    if let Some(ctx) = prior_context {
        s.push_str(ctx);
    }
    if !attached.is_empty() {
        s.push_str("The following files have been attached for context:\n\n");
        for (path, content) in attached {
            s.push_str(&format!("[{path}]\n{content}\n\n"));
        }
        s.push_str("---\n\n");
    }
    s.push_str(task);
    s
}

// ── Think-tag streaming parser ────────────────────────────────────────────────

/// Splits a streaming text into normal chunks and `<think>…</think>` thinking chunks.
///
/// Models like DeepSeek emit `<think>` blocks mid-stream. The parser buffers a
/// small lookahead window so tags split across chunk boundaries are handled
/// correctly. Call `push(chunk)` for each incoming chunk; it returns the normal
/// text and thinking text that can be flushed safely at that point. Call
/// `finish()` once the stream ends to drain any remainder.
pub struct ThinkParser {
    buf: String,
    in_think: bool,
}

impl ThinkParser {
    pub fn new() -> Self {
        Self { buf: String::new(), in_think: false }
    }

    /// Feed a new chunk. Returns `(normal_text, thinking_text)` ready to emit.
    pub fn push(&mut self, chunk: &str) -> (String, String) {
        self.buf.push_str(chunk);
        let mut normal = String::new();
        let mut thinking = String::new();

        loop {
            if self.in_think {
                if let Some(pos) = self.buf.find("</think>") {
                    thinking.push_str(&self.buf[..pos]);
                    self.buf = self.buf[pos + 8..].to_string();
                    self.in_think = false;
                } else {
                    // Flush all but the last 8 bytes (tag might be split across chunks).
                    let keep = self.buf.floor_char_boundary(self.buf.len().saturating_sub(8));
                    if keep > 0 {
                        thinking.push_str(&self.buf[..keep]);
                        self.buf = self.buf[keep..].to_string();
                    }
                    break;
                }
            } else if let Some(pos) = self.buf.find("<think>") {
                normal.push_str(&self.buf[..pos]);
                self.buf = self.buf[pos + 7..].to_string();
                self.in_think = true;
            } else {
                // Flush all but the last 7 bytes (opening tag might be split).
                let keep = self.buf.floor_char_boundary(self.buf.len().saturating_sub(7));
                if keep > 0 {
                    normal.push_str(&self.buf[..keep]);
                    self.buf = self.buf[keep..].to_string();
                }
                break;
            }
        }

        (normal, thinking)
    }

    /// Drain any buffered remainder after the stream ends.
    pub fn finish(mut self) -> (String, String) {
        let remainder = std::mem::take(&mut self.buf);
        if self.in_think {
            (String::new(), remainder)
        } else {
            (remainder, String::new())
        }
    }
}


// ── Tool call execution ───────────────────────────────────────────────────────

/// Execute a single tool call, applying all guards, cache, dispatch, history,
/// hooks, and stale-eviction in one focused function.
///
/// Returns `None` when the result was already pushed directly into `tool_results`
/// by an early-continue path (dependency guard stub, cache hit). Returns
/// `Some(ContentPart)` for the caller to collect into `tool_results`.
/// The bool indicates whether a real dispatch occurred (i.e. not a cache hit or stub) —
/// callers use this to increment the tool call counter accurately.
async fn execute_one_tool_call(
    tc: &ToolCall,
    mutated_files: &mut std::collections::HashSet<String>,
    messages: &mut Vec<Message>,
    cache: &mut FileCache,
    history: &mut History,
    loop_detector: &mut LoopDetector,
    config: &AgentConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> (Option<ContentPart>, bool) {
    // ── Parse args ────────────────────────────────────────────────────────────
    let args: Value = match serde_json::from_str(&tc.arguments) {
        Ok(v) => v,
        Err(e) => {
            return (Some(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: format!("[Error parsing tool arguments: {e}]"),
            }), false);
        }
    };

    // Classify once — used by cache, mutation guard, hooks, and stale eviction.
    let kind = ToolKind::classify(&tc.name, &args);

    // ── Dependency guard ──────────────────────────────────────────────────────
    // If the model batches multiple mutations on the same file, stub all but
    // the first. Prevents speculative chaining with stale line numbers/hashes.
    if let ToolKind::Mutate { ref path } = kind {
        if mutated_files.contains(path) {
            let _ = ui_tx.send(UiEvent::ToolResult {
                summary: format!("⚠ skipped dependent edit on {path}"),
            });
            return (Some(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: format!(
                    "[Not executed: '{path}' was already modified by an earlier call in \
                     this batch. Re-plan this edit after seeing that result — use fresh \
                     line numbers and hashes from the post-edit context above.]"
                ),
            }), false);
        }
    }

    // ── Loop detection + dry-run + cache + dispatch ───────────────────────────
    let mut dispatched = true;
    let mut result_content = if loop_detector.record(&tc.name, &tc.arguments) {
        dispatched = false;
        let _ = ui_tx.send(UiEvent::LoopWarning { tool_name: tc.name.clone() });
        format!(
            "[Loop detected: {} called with identical arguments. \
             Try a different approach or more specific arguments.]",
            tc.name
        )
    } else if config.dry_run {
        dispatched = false;
        format!("[dry-run: {} not executed]", tc.name)
    } else {
        // ── Cache read (before dispatch) ──────────────────────────────────────
        if let ToolKind::Read { ref path, is_symbols: false, .. } = kind {
            // Full read hit
            if let ToolKind::Read { has_range: false, .. } = kind {
                if let Some(hit) = cache.check(path) {
                    let _ = ui_tx.send(UiEvent::CacheHit { path: path.clone(), lines: hit.total_lines });
                    let output = hit.into_message();
                    let (model_output, _) = history.record(&tc.id, &tc.name, &output);
                    return (Some(ContentPart::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: model_output,
                    }), false);
                }
            }
            // Ranged read hit — serve slice from cached lines
            if let ToolKind::Read { has_range: true, .. } = kind {
                if let Some(range) = args["line_range"].as_array() {
                    let start = range.first().and_then(|v| v.as_u64())
                        .map(|n| (n as usize).saturating_sub(1)).unwrap_or(0);
                    let end = range.get(1).and_then(|v| v.as_u64())
                        .map(|n| n as usize).unwrap_or(usize::MAX);
                    if let Some(hit) = cache.check_range(path, start, end) {
                        let _ = ui_tx.send(UiEvent::CacheHit { path: path.clone(), lines: hit.range_lines });
                        let output = hit.into_message();
                        let (model_output, _) = history.record(&tc.id, &tc.name, &output);
                        return (Some(ContentPart::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: model_output,
                        }), false);
                    }
                }
            }
        }

        // ── Dispatch ──────────────────────────────────────────────────────────
        let _ = ui_tx.send(UiEvent::ToolCall {
            name: tc.name.clone(),
            args_summary: format_args_summary(&args),
        });
        let raw = dispatch_tool(&tc.name, &args, &kind, cache, history, ui_tx, &config.mcp).await;

        // ── Post-dispatch cache maintenance ───────────────────────────────────
        match &kind {
            ToolKind::Read { path, has_range: false, is_symbols: false } => {
                // Full read — cache the raw content so ranged re-reads are also free.
                // We re-read from disk here to get the raw lines (raw is already formatted output).
                if let Ok(content) = std::fs::read_to_string(path) {
                    cache.store(path, content);
                }
            }
            ToolKind::Read { path, has_range: true, is_symbols: false } => {
                // Ranged read miss — prime the cache with the full file so future
                // reads of any window (including different ranges) are free.
                if let Ok(content) = std::fs::read_to_string(path) {
                    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                    cache.store_lines(path, lines);
                }
            }
            ToolKind::Mutate { path } => {
                cache.invalidate(path);
            }
            ToolKind::Other { is_bash: true } => {
                if let Some(cmd) = args["command"].as_str() {
                    cache.invalidate_if_mentioned(cmd);
                }
            }
            _ => {}
        }

        raw
    };

    // ── History + display ─────────────────────────────────────────────────────
    let (model_output, display_summary) = history.record(&tc.id, &tc.name, &result_content);
    let _ = ui_tx.send(UiEvent::ToolResult { summary: display_summary });
    if config.verbose {
        for line in model_output.lines().skip(1).take(4) {
            let _ = ui_tx.send(UiEvent::ToolResult { summary: format!("  {line}") });
        }
    }
    result_content = model_output;

    // ── on_edit hooks ─────────────────────────────────────────────────────────
    // Appended into the tool result so the model sees compile/lint errors
    // immediately and can self-correct without an extra round-trip.
    if let ToolKind::Mutate { .. } = kind {
        for hr in hooks::run_edit_hooks(&config.hooks, config.hooks_enabled).await {
            let hook_line = if hr.exit_code == 0 && hr.output.trim().is_empty() {
                format!("\n\n⚙ `{}` ✓", hr.cmd)
            } else {
                format!("\n\n⚙ `{}` (exit {}):\n{}", hr.cmd, hr.exit_code, hr.output)
            };
            result_content.push_str(&hook_line);
            let _ = ui_tx.send(UiEvent::HookOutput {
                event: "on_edit".to_string(),
                output: hr.output,
                exit_code: hr.exit_code,
            });
        }
    }

    // ── Post-mutation stale eviction ──────────────────────────────────────────
    if let ToolKind::Mutate { ref path } = kind {
        if !result_content.contains("[Tool error") {
            history.compress_reads_for(path);
            evict_stale_content(messages, path);
        }
        mutated_files.insert(path.clone());
    }

    (Some(ContentPart::ToolResult {
        tool_use_id: tc.id.clone(),
        content: result_content,
    }), dispatched)
}

/// Strip CoT reasoning text from the most recent assistant turn that has tool
/// calls. Once tool results are in, the reasoning text only wastes context.
/// Final assistant turns (no tool calls) are never touched.
fn strip_cot_from_last_assistant(messages: &mut Vec<Message>) {
    if let Some(asst_msg) = messages.iter_mut().rev().nth(1) {
        if asst_msg.role == "assistant" && !asst_msg.tool_calls.is_empty() {
            asst_msg.content = MessageContent::from(String::new());
        }
    }
}

// ── Stale content eviction ────────────────────────────────────────────────────

/// After a file is edited, evict ALL stale content referencing that path from
/// the messages array. This covers:
///   - read_file results (full file content with now-wrong hashes/line numbers)
///   - edit_file post-edit echoes (±10 line excerpts with now-wrong hashes)
///   - search results that include matches from this file
///
/// Stale content is actively harmful: wrong hashes cause anchor mismatches,
/// wrong line numbers cause failed edits, wrong code causes incorrect old_str.
fn evict_stale_content(messages: &mut [Message], edited_path: &str) {
    let stub = format!("[Stale — {edited_path} was edited. Re-read for current content.]");
    for msg in messages.iter_mut() {
        if msg.role != "tool" {
            continue;
        }
        if let MessageContent::Parts(parts) = &mut msg.content {
            for part in parts.iter_mut() {
                if let ContentPart::ToolResult { content, .. } = part {
                    // Skip already-short content — nothing to save
                    if content.len() <= 150 || !content.contains(edited_path) {
                        continue;
                    }

                    // read_file output: "[path" header + numbered lines
                    let is_read = content.starts_with('[')
                        && (content.contains(" | ") || content.contains(" — "));

                    // edit_file post-edit echo: "✓ Edited path" + excerpt with hashes
                    let is_edit_echo = content.contains("✓ Edited")
                        && content.contains(" | ");

                    // search results referencing this file
                    // (rg format: "path:line:content")
                    let is_search = {
                        let prefix = format!("{edited_path}:");
                        content.lines().any(|l| l.starts_with(&prefix))
                    };

                    if is_read || is_edit_echo || is_search {
                        *content = stub.clone();
                    }
                }
            }
        }
    }
}

// ── Tool kind classification ───────────────────────────────────────────────────
//
// Parsed once per tool call so the loop can make cache/mutation decisions
// without re-parsing arguments or scattering tool-name strings across the code.

enum ToolKind {
    /// read_file — may be served from / stored into cache.
    Read { path: String, has_range: bool, is_symbols: bool },
    /// edit_file / write_file / patch_file — mutates a file path.
    Mutate { path: String },
    /// Everything else. `is_bash` flags bash for post-run cache invalidation.
    Other { is_bash: bool },
}

impl ToolKind {
    fn classify(name: &str, args: &Value) -> Self {
        match name {
            "read_file" => ToolKind::Read {
                path: args["path"].as_str().unwrap_or("").to_string(),
                has_range: !args["line_range"].is_null(),
                is_symbols: args["symbols"].as_bool().unwrap_or(false),
            },
            "edit_file" | "write_file" | "patch_file" => ToolKind::Mutate {
                path: args["path"].as_str().unwrap_or("").to_string(),
            },
            "bash" => ToolKind::Other { is_bash: true },
            _ => ToolKind::Other { is_bash: false },
        }
    }
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────
//
// Pure routing: parse → call → return raw string.
// Cache, history, hooks, dry-run and UI events are all handled by the caller.

async fn dispatch_tool(
    name: &str,
    args: &Value,
    kind: &ToolKind,
    cache: &mut FileCache,
    history: &History,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    mcp: &McpClient,
) -> String {
    match name {
        "bash"     => tools::bash::execute(args).await.unwrap_or_else(|e| format!("[Tool error: {e}]")),
        "search"   => tools::search::execute(args).await.unwrap_or_else(|e| format!("[Tool error: {e}]")),
        "recall"   => tools::recall::execute(args, history).unwrap_or_else(|e| e),
        "ask_user" => tools::ask::execute(args, ui_tx.clone()).await.unwrap_or_else(|e| e),
        "edit_file" | "write_file" | "patch_file" => {
            match tools::dispatch(name, args) {
                Ok(o) => o,
                Err(e) => {
                    // On edit failure, show the current file content so the model
                    // can correct its old_str without an extra round-trip.
                    let path = args["path"].as_str().unwrap_or("");
                    let hint = if let ToolKind::Mutate { .. } = kind {
                        if let Some(hit) = cache.check(path) {
                            format!("\n\nCurrent file content for reference:\n{}", hit.content)
                        } else if let Ok(content) = std::fs::read_to_string(path) {
                            format!("\n\nCurrent file content for reference:\n{content}")
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    format!("[Tool error: {e}]{hint}")
                }
            }
        }
        _ => {
            if tools::is_native(name) {
                tools::dispatch(name, args).unwrap_or_else(|e| format!("[Tool error: {e}]"))
            } else {
                mcp.call(name, args.clone()).await.unwrap_or_else(|e| format!("[MCP tool error: {e}]"))
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn format_args_summary(args: &Value) -> String {
    if let Some(obj) = args.as_object() {
        let pairs: Vec<String> = obj
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    Value::String(s) => {
                        if s.chars().count() > 60 {
                            let truncated: String = s.chars().take(57).collect();
                            format!("\"{}…\"", truncated)
                        } else {
                            format!("\"{s}\"")
                        }
                    }
                    other => {
                        let s = other.to_string();
                        if s.chars().count() > 40 {
                            let truncated: String = s.chars().take(37).collect();
                            format!("{}…", truncated)
                        } else {
                            s
                        }
                    }
                };
                format!("{k}={val}")
            })
            .collect();
        pairs.join(", ")
    } else {
        args.to_string()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_user_message ────────────────────────────────────────────────────

    #[test]
    fn test_user_message_task_only() {
        let msg = build_user_message("do the thing", None, &[]);
        assert_eq!(msg, "do the thing");
    }

    #[test]
    fn test_user_message_with_prior_context() {
        let msg = build_user_message("new task", Some("prior stuff\n"), &[]);
        assert!(msg.starts_with("prior stuff\n"));
        assert!(msg.ends_with("new task"));
    }

    #[test]
    fn test_user_message_with_attached_files() {
        let attached = vec![
            ("src/foo.rs".to_string(), "fn foo() {}".to_string()),
        ];
        let msg = build_user_message("fix it", None, &attached);
        assert!(msg.contains("[src/foo.rs]"));
        assert!(msg.contains("fn foo() {}"));
        assert!(msg.contains("---"));
        assert!(msg.ends_with("fix it"));
    }

    #[test]
    fn test_user_message_ordering() {
        // prior context → attached files → task, in that order
        let attached = vec![("a.rs".to_string(), "content".to_string())];
        let msg = build_user_message("task", Some("prior"), &attached);
        let prior_pos = msg.find("prior").unwrap();
        let attach_pos = msg.find("[a.rs]").unwrap();
        let task_pos = msg.rfind("task").unwrap();
        assert!(prior_pos < attach_pos);
        assert!(attach_pos < task_pos);
    }

    // ── build_system_prompt ───────────────────────────────────────────────────

    fn minimal_config() -> AgentConfig {
        // McpClient::new is async; construct a dummy via the blocking runtime.
        let mcp = tokio::runtime::Handle::try_current()
            .map(|h| h.block_on(crate::mcp::McpClient::new(&[])))
            .unwrap_or_else(|_| {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(crate::mcp::McpClient::new(&[]))
            });
        AgentConfig {
            verbose: false,
            dry_run: false,
            context_tokens: 8000,
            _profile_name: String::new(),
            _model: String::new(),
            _show_timestamps: false,
            mcp,
            hooks: std::sync::Arc::new(crate::hooks::HookConfig::default()),
            hooks_enabled: false,
            auto_commit: false,
            auto_commit_prefix: String::new(),
            git_context: false,
            project_context: None,
        }
    }

    #[test]
    fn test_system_prompt_contains_base() {
        let config = minimal_config();
        let prompt = build_system_prompt(&config, None);
        assert!(prompt.contains("PareCode"));
    }

    #[test]
    fn test_system_prompt_includes_project_context() {
        let mut config = minimal_config();
        config.project_context = Some("# Clusters\nfoo bar".to_string());
        let prompt = build_system_prompt(&config, None);
        assert!(prompt.contains("# Clusters"));
        assert!(prompt.contains("foo bar"));
    }

    #[test]
    fn test_system_prompt_git_status_included() {
        let config = minimal_config();
        let prompt = build_system_prompt(&config, Some("M src/foo.rs"));
        assert!(prompt.contains("# Git status"));
        assert!(prompt.contains("M src/foo.rs"));
    }

    #[test]
    fn test_system_prompt_git_status_empty_skipped() {
        let config = minimal_config();
        let prompt = build_system_prompt(&config, Some("   "));
        assert!(!prompt.contains("# Git status"));
    }

    #[test]
    fn test_system_prompt_no_git_status() {
        let config = minimal_config();
        let prompt = build_system_prompt(&config, None);
        assert!(!prompt.contains("# Git status"));
    }

    // ── ThinkParser ───────────────────────────────────────────────────────────

    #[test]
    fn test_think_parser_no_tags() {
        let mut p = ThinkParser::new();
        let (normal, thinking) = p.push("hello world");
        // Some held back as lookahead — drain with finish()
        let (n2, t2) = p.finish();
        assert_eq!(normal + &n2, "hello world");
        assert!(thinking.is_empty() && t2.is_empty());
    }

    #[test]
    fn test_think_parser_full_think_block() {
        let mut p = ThinkParser::new();
        let (n1, t1) = p.push("before<think>reasoning</think>after");
        let (n2, t2) = p.finish();
        assert_eq!(n1 + &n2, "beforeafter");
        assert_eq!(t1 + &t2, "reasoning");
    }

    #[test]
    fn test_think_parser_tag_split_across_chunks() {
        let mut p = ThinkParser::new();
        // "<think>" split as "<thi" + "nk>"
        let (n1, t1) = p.push("hello<thi");
        let (n2, t2) = p.push("nk>reasoning</think>done");
        let (n3, t3) = p.finish();
        assert_eq!(n1 + &n2 + &n3, "hellodone");
        assert_eq!(t1 + &t2 + &t3, "reasoning");
    }

    #[test]
    fn test_think_parser_close_tag_split_across_chunks() {
        let mut p = ThinkParser::new();
        let (n1, t1) = p.push("<think>thinking</thi");
        let (n2, t2) = p.push("nk>normal");
        let (n3, t3) = p.finish();
        assert_eq!(n1 + &n2 + &n3, "normal");
        assert!((t1 + &t2 + &t3).contains("thinking"));
    }

    #[test]
    fn test_think_parser_multiple_blocks() {
        let mut p = ThinkParser::new();
        let input = "a<think>t1</think>b<think>t2</think>c";
        let (n1, t1) = p.push(input);
        let (n2, t2) = p.finish();
        assert_eq!(n1 + &n2, "abc");
        assert_eq!(t1 + &t2, "t1t2");
    }

    #[test]
    fn test_think_parser_multibyte_chars() {
        // em-dash (3 bytes in UTF-8) should not panic when split by the lookahead window
        let mut p = ThinkParser::new();
        let (n1, _) = p.push("hello — world");
        let (n2, _) = p.finish();
        assert_eq!(n1 + &n2, "hello — world");
    }

    #[test]
    fn test_think_parser_unclosed_tag_goes_to_thinking_on_finish() {
        let mut p = ThinkParser::new();
        let (_, t1) = p.push("<think>half done");
        let (normal, t2) = p.finish();
        assert!(normal.is_empty());
        assert!((t1 + &t2).contains("half done"));
    }

    // ── strip_cot_from_last_assistant ─────────────────────────────────────────

    #[test]
    fn test_strip_cot_strips_assistant_with_tool_calls() {
        let tool_call = crate::client::ToolCall {
            id: "tc1".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        };
        let mut messages = vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::from("task".to_string()),
                tool_calls: vec![],
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::from("I will now read the file...".to_string()),
                tool_calls: vec![tool_call],
            },
            Message {
                role: "tool".to_string(),
                content: MessageContent::from("file content".to_string()),
                tool_calls: vec![],
            },
        ];
        strip_cot_from_last_assistant(&mut messages);
        // The assistant turn (index 1, second from last) should have empty text
        if let MessageContent::Text(t) = &messages[1].content {
            assert!(t.is_empty(), "CoT text should be stripped");
        }
    }

    #[test]
    fn test_strip_cot_does_not_touch_final_assistant_turn() {
        // Final assistant turn has no tool calls — must not be stripped
        let mut messages = vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::from("task".to_string()),
                tool_calls: vec![],
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::from("Here is my answer.".to_string()),
                tool_calls: vec![],
            },
        ];
        strip_cot_from_last_assistant(&mut messages);
        if let MessageContent::Text(t) = &messages[1].content {
            assert_eq!(t, "Here is my answer.", "final turn must be untouched");
        }
    }

    // ── execute_one_tool_call ─────────────────────────────────────────────────

    async fn minimal_config_async() -> AgentConfig {
        AgentConfig {
            verbose: false,
            dry_run: false,
            context_tokens: 8000,
            _profile_name: String::new(),
            _model: String::new(),
            _show_timestamps: false,
            mcp: crate::mcp::McpClient::new(&[]).await,
            hooks: std::sync::Arc::new(crate::hooks::HookConfig::default()),
            hooks_enabled: false,
            auto_commit: false,
            auto_commit_prefix: String::new(),
            git_context: false,
            project_context: None,
        }
    }

    fn make_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    fn make_channel() -> (mpsc::UnboundedSender<UiEvent>, mpsc::UnboundedReceiver<UiEvent>) {
        mpsc::unbounded_channel()
    }

    fn collect_events(rx: &mut mpsc::UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
        let mut events = vec![];
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    #[tokio::test]
    async fn test_execute_one_tool_call_parse_error() {
        let config = minimal_config_async().await;
        let tc = make_tool_call("id1", "read_file", "not valid json{{");
        let (tx, mut rx) = make_channel();
        let (result, dispatched) = execute_one_tool_call(
            &tc,
            &mut Default::default(),
            &mut vec![],
            &mut FileCache::default(),
            &mut History::default(),
            &mut LoopDetector::default(),
            &config,
            &tx,
        ).await;
        assert!(!dispatched, "parse error should not count as dispatched");
        let part = result.expect("should return Some on parse error");
        if let ContentPart::ToolResult { content, .. } = part {
            assert!(content.contains("Error parsing tool arguments"), "got: {content}");
        } else {
            panic!("expected ToolResult");
        }
        // No UI events emitted for parse errors
        let events = collect_events(&mut rx);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_execute_one_tool_call_dependency_guard() {
        let config = minimal_config_async().await;
        let tc = make_tool_call("id2", "edit_file", r#"{"path":"src/foo.rs"}"#);
        let (tx, mut rx) = make_channel();
        let mut mutated = std::collections::HashSet::new();
        mutated.insert("src/foo.rs".to_string());

        let (result, dispatched) = execute_one_tool_call(
            &tc,
            &mut mutated,
            &mut vec![],
            &mut FileCache::default(),
            &mut History::default(),
            &mut LoopDetector::default(),
            &config,
            &tx,
        ).await;
        assert!(!dispatched, "dependency guard stub should not count as dispatched");
        let part = result.expect("dependency guard returns Some stub");
        if let ContentPart::ToolResult { content, .. } = part {
            assert!(content.contains("Not executed"), "got: {content}");
            assert!(content.contains("src/foo.rs"));
        }
        let events = collect_events(&mut rx);
        assert!(events.iter().any(|e| matches!(e, UiEvent::ToolResult { .. })));
    }

    #[tokio::test]
    async fn test_execute_one_tool_call_loop_detection() {
        let config = minimal_config_async().await;
        let tc = make_tool_call("id3", "bash", r#"{"command":"ls"}"#);
        let (tx, mut rx) = make_channel();
        let mut detector = LoopDetector::default();
        // Prime the detector with one call
        detector.record("bash", r#"{"command":"ls"}"#);

        let (result, dispatched) = execute_one_tool_call(
            &tc,
            &mut Default::default(),
            &mut vec![],
            &mut FileCache::default(),
            &mut History::default(),
            &mut detector,
            &config,
            &tx,
        ).await;
        assert!(!dispatched, "loop detection should not count as dispatched");
        let part = result.expect("loop detection returns Some");
        if let ContentPart::ToolResult { content, .. } = part {
            assert!(content.contains("Loop detected"), "got: {content}");
        }
        let events = collect_events(&mut rx);
        assert!(events.iter().any(|e| matches!(e, UiEvent::LoopWarning { .. })));
    }

    #[tokio::test]
    async fn test_execute_one_tool_call_dry_run() {
        let mut config = minimal_config_async().await;
        config.dry_run = true;
        let tc = make_tool_call("id4", "bash", r#"{"command":"rm -rf /"}"#);
        let (tx, _rx) = make_channel();

        let (result, dispatched) = execute_one_tool_call(
            &tc,
            &mut Default::default(),
            &mut vec![],
            &mut FileCache::default(),
            &mut History::default(),
            &mut LoopDetector::default(),
            &config,
            &tx,
        ).await;
        assert!(!dispatched, "dry-run should not count as dispatched");
        let part = result.expect("dry_run returns Some");
        if let ContentPart::ToolResult { content, .. } = part {
            assert!(content.contains("dry-run"), "got: {content}");
            assert!(content.contains("bash"));
        }
    }
}
