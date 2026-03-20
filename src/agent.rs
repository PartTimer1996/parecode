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

const SYSTEM_PROMPT_BASE: &str = "You are PareCode, a coding assistant. \
Complete tasks using the available tools in minimum tool calls. \
A project index is pre-loaded — use project_index for symbol locations before read_file. \
When done, stop.";

/// Quick mode — single API call, no multi-turn loop, minimal context.
/// Targets < 2k tokens total. No file loading, no session history.
/// Allows at most 1 tool call before returning (edit_file, bash read-only).
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
If a tool call is needed, make exactly one — prefer edit_file or bash. \
Do not read files unless strictly necessary. Keep responses short.";

    // Lean tool list — only the tools that make sense for quick tasks
    let quick_tools: Vec<crate::client::Tool> = tools::all_definitions()
        .into_iter()
        .filter(|t| matches!(t.name.as_str(), "edit_file" | "read_file" | "bash"))
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
        let _ = ui_tx.send(UiEvent::ToolCall {
            name: tc.name.clone(),
            args_summary: format_args_summary(&args),
        });
        let raw = dispatch_tool(&tc.name, &args, &kind, &mut cache, &ui_tx, &config.mcp, config).await;
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
/// `attached` is a list of file paths hinted by the user via #file — no content, just pointers.
/// `prior_turns` is the last N completed turns from this session, prepended as message pairs.
pub async fn run_tui(
    task: &str,
    client: &Client,
    config: &AgentConfig,
    attached: Vec<String>,
    prior_turns: Vec<crate::sessions::ConversationTurn>,
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
    // Cache the tool list — only rebuild when the phase actually changes.
    // Tool schemas are ~2,600 tokens; rebuilding every turn wastes nothing but
    // the Vec alloc — the real saving is sending the same bytes as a pointer.
    // More importantly this makes the phase key explicit and auditable.
    let mut cached_tools: Option<(/*key:*/ (usize, bool), Vec<Tool>)> = None;

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

    let user_content = build_user_message(task, &attached);

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

    // ── PIE injection ────────────────────────────────────────────────────────
    // Prepend a synthetic project_index tool result so the model sees the
    // project summary as pre-executed context (high salience, sent once).
    if let Some(graph) = &config.project_graph {
        messages.extend(crate::pie::pie_injection_messages(
            task,
            graph,
            config.project_narrative.as_deref(),
            &attached,
        ));
    }

    // ── Prior turn pairs ─────────────────────────────────────────────────────
    // Inject the last few turns as lean user/assistant pairs so the model has
    // session continuity for follow-up tasks. No tool results — just what was
    // asked and what was answered. Capped to avoid token bloat.
    const MAX_PRIOR_TURNS: usize = 3;
    let prior_start = prior_turns.len().saturating_sub(MAX_PRIOR_TURNS);
    for turn in &prior_turns[prior_start..] {
        let user_text: String = turn.user_message.chars().take(300).collect();
        let assistant_text: String = turn.agent_response.chars().take(500).collect();
        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::from(user_text),
            tool_calls: vec![],
        });
        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::from(assistant_text),
            tool_calls: vec![],
        });
    }

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
        // Only rebuild the tool list when the phase key changes — saves ~50µs of
        // Vec allocation per turn. The tool schemas themselves are not re-sent
        // (HTTP body is built fresh each call); this just avoids redundant work.
        let has_graph = config.project_graph.is_some();
        let tool_key = (turn, has_graph);
        let tools = match &cached_tools {
            Some((key, t)) if *key == tool_key => t.clone(),
            _ => {
                let mut t = tools::tools_for_turn(turn, has_graph);
                t.extend(mcp_tool_defs.iter().cloned());
                cached_tools = Some((tool_key, t.clone()));
                t
            }
        };

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

        // Strip <think>…</think> CoT blocks from the stored assistant message.
        // response.text may contain raw <think> tags when the model emits reasoning
        // in the content field (not reasoning_content). ThinkParser already strips
        // these for TUI display — do the same before persisting to message history.
        let stored_text = {
            let mut p = ThinkParser::new();
            let (mut normal, _) = p.push(&response.text);
            let (n2, _) = p.finish();
            normal.push_str(&n2);
            normal
        };
        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::from(stored_text),
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
        // let mut any_bash_or_ask = false; // bash or ask_user ran → model needs to see output
        // let mut any_error = false;       // any result contained an error/failure marker

        for tc in &response.tool_calls {
            let parsed_args = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
            let kind = ToolKind::classify(&tc.name, &parsed_args);
            let _is_observe_kind = matches!(kind, ToolKind::Other { .. }) || tc.name == "ask_user";
            // if is_observe_kind {
            //     any_bash_or_ask = true;
            // }
            let (part, dispatched, _had_error) = execute_one_tool_call(
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
            // if had_error {
            //     any_error = true;
            // }
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

        // Below is still not working really - the aim is to just skip the conclusion, maybe they're valuable 
        // Potentially we could check for the "stop" mechanism and trim it down instead - basically reduce tokens for this procedure? 
        // ── Early-exit: skip the "I'm done" round-trip ────────────────────────
        // Mutations succeeded, no bash/ask_user that needs a follow-up response.
        // Reads are fine — the model already consumed them to make the edits.
        // if should_skip_done_turn(!mutated_files.is_empty(), any_bash_or_ask, any_error) {
        //     break;
        // }

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
    /// Project graph for PIE injection and graph intercept. None for executor plan steps.
    pub project_graph: Option<std::sync::Arc<crate::pie::ProjectGraph>>,
    /// Project narrative for PIE injection. None for executor plan steps.
    pub project_narrative: Option<std::sync::Arc<crate::narrative::ProjectNarrative>>,
}

// ── Pure prompt-assembly helpers ──────────────────────────────────────────────

/// Assemble the full system prompt from base + conventions + git status.
/// PIE context is no longer injected here — it goes into the message history
/// as a synthetic tool result via `pie_injection_messages()`.
/// `git_status` is pre-fetched by the caller so this function stays pure and testable.
pub fn build_system_prompt(config: &AgentConfig, git_status: Option<&str>) -> String {
    let _ = config; // config retained for future extensions (model tier, etc.)
    let mut prompt = SYSTEM_PROMPT_BASE.to_string();

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

/// Assemble the user message: attached file hints + task.
/// `attached` is paths only — no content. The model uses read_file if it needs content.
pub fn build_user_message(task: &str, attached: &[String]) -> String {
    let mut s = String::new();
    if !attached.is_empty() {
        s.push_str("Relevant files (use read_file if you need content):\n");
        for path in attached {
            s.push_str(&format!("- {path}\n"));
        }
        s.push('\n');
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
/// The second bool indicates whether a real dispatch occurred (not a cache hit or stub).
/// The third bool indicates whether the result contains an error or hook failure —
/// computed after on_edit hooks fire, giving the caller a complete error signal.
async fn execute_one_tool_call(
    tc: &ToolCall,
    mutated_files: &mut std::collections::HashSet<String>,
    messages: &mut Vec<Message>,
    cache: &mut FileCache,
    history: &mut History,
    loop_detector: &mut LoopDetector,
    config: &AgentConfig,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> (Option<ContentPart>, bool, bool) {
    // ── Parse args ────────────────────────────────────────────────────────────
    let args: Value = match serde_json::from_str(&tc.arguments) {
        Ok(v) => v,
        Err(e) => {
            return (Some(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: format!("[Error parsing tool arguments: {e}]"),
            }), false, true);
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
            }), false, true); // treat skipped edit as error — model needs to re-plan
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
                    let (model_output, _) = history.record(&tc.name, &output);
                    return (Some(ContentPart::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: model_output,
                    }), false, false);
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
                        let (model_output, _) = history.record( &tc.name, &output);
                        return (Some(ContentPart::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: model_output,
                        }), false, false);
                    }
                }
            }
        }

        // ── Dispatch ──────────────────────────────────────────────────────────
        let _ = ui_tx.send(UiEvent::ToolCall {
            name: tc.name.clone(),
            args_summary: format_args_summary(&args),
        });
        let raw = dispatch_tool(&tc.name, &args, &kind, cache, ui_tx, &config.mcp, config).await;

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
    let (model_output, display_summary) = history.record(&tc.name, &result_content);
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

    // Compute had_error after hooks have been appended — this is the authoritative signal
    // for the early-exit logic. Catches: tool errors, build failures, hook failures (exit != 0).
    let had_error = result_content.contains("[Tool error")
        || result_content.contains("[Not executed")
        || result_content.contains("✗ build check failed")
        || result_content.contains("⚠ FILE WRITTEN BUT BUILD BROKEN")
        || result_content.contains("⚠ skipped dependent edit")
        || result_content.contains("[Loop detected")
        || result_content.contains("[dry-run")
        || (result_content.contains("(exit ") && {
            // Hook exited non-zero: pattern is `⚙ `cmd` (exit N):` where N != 0
            result_content.lines().any(|l| {
                if let Some(rest) = l.strip_prefix("⚙ ") {
                    if let Some(idx) = rest.find("(exit ") {
                        let after = &rest[idx + 6..];
                        let code: i32 = after.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
                        return code != 0;
                    }
                }
                false
            })
        });

    (Some(ContentPart::ToolResult {
        tool_use_id: tc.id.clone(),
        content: result_content,
    }), dispatched, had_error)
}

/// Per-turn context compression — runs after each tool round-trip, before the
/// next API call.
///
/// 1. Strip CoT text from ALL intermediate assistant turns (those with tool_calls).
///    Once results are in, the reasoning is consumed and wastes tokens forever.
///    Final assistant turns (no tool calls) are the visible response — never touched.
///
/// 2. Compress ALL tool result messages except the most recent one.
///    bash/read results are consumed the moment the model acts on them. Keeping
///    full content in history re-pays the token cost on every subsequent API call.
///    The last tool message is left intact — model may still need it this turn.
fn strip_cot_from_last_assistant(messages: &mut Vec<Message>) {
    let last_idx = messages.len().saturating_sub(1);

    // Pass 1: clear CoT from intermediate assistant turns
    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.role == "assistant" && !msg.tool_calls.is_empty() && i < last_idx {
            msg.content = MessageContent::from(String::new());
        }
    }

    // Pass 2: compress older tool messages to one-liner stubs.
    // The last tool message is protected — model just received it.
    // Everything before that is consumed history.
    let last_tool_idx = messages.iter().rposition(|m| m.role == "tool").unwrap_or(0);

    // Build tool_use_id → tool_name map from all assistant messages so we can
    // make compression decisions based on which tool produced a result.
    // Use owned Strings to avoid borrowing messages across the mutable loop below.
    let id_to_name: std::collections::HashMap<String, String> = messages.iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.iter().map(|tc| (tc.id.clone(), tc.name.clone())))
        .collect();

    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.role != "tool" || i >= last_tool_idx {
            continue;
        }
        if let MessageContent::Parts(parts) = &mut msg.content {
            for part in parts.iter_mut() {
                if let ContentPart::ToolResult { tool_use_id, content } = part {
                    let tool_name = id_to_name.get(tool_use_id.as_str()).map(|s| s.as_str()).unwrap_or("");
                    if should_protect_tool_result(tool_name, content) {
                        continue;
                    }
                    *content = compress_tool_result_to_stub(content);
                }
            }
        }
    }
}

/// Returns true if this tool result should be kept verbatim in history.
/// Navigation results (find_symbol, graph intercepts) are tiny and permanently
/// valid — compressing them causes re-lookups. Small ranged reads are kept
/// because the model explicitly requested that window and likely still needs it.
fn should_protect_tool_result(tool_name: &str, content: &str) -> bool {
    // find_symbol results — always tiny (~15-80 tokens), always valid navigation data
    if tool_name == "find_symbol" {
        return true;
    }
    // Ranged read_file results — always retain verbatim.
    // The model explicitly requested this window (via find_symbol → line_range).
    // Compressing it forces a re-read at higher total token cost than retention.
    // Stale content after edits is handled separately by evict_stale_content().
    if tool_name == "read_file" {
        let first = content.lines().next().unwrap_or("");
        if first.contains("— lines ") && first.contains(" of ") {
            return true;
        }
    }
    // Already a stub or tiny result — no-op
    if content.len() <= 120 {
        return true;
    }
    false
}

/// Compress a consumed tool result to a minimal stub.
/// Keeps enough for the model to know what ran; discards the bulk.
fn compress_tool_result_to_stub(content: &str) -> String {
    let first = content.lines().next().unwrap_or(content);

    // read_file: "[src/foo.rs — 500 lines total...]" → keep header only
    if first.starts_with('[') && (first.contains(" — ") || first.contains(" lines")) {
        let inner = first.trim_matches(|c| c == '[' || c == ']');
        let path = inner.split(" —").next().unwrap_or(inner).trim();
        let line_count = content.lines().filter(|l| l.contains(" | ")).count();
        if line_count > 0 {
            return format!("[read {path} — {line_count} lines shown, consumed]");
        }
        return format!("[read {path} — consumed]");
    }

    // bash: keep first meaningful line as evidence of what ran
    let summary = content.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(first);
    format!("[bash result consumed: {}{}]",
        &summary[..summary.len().min(80)],
        if summary.len() > 80 { "…" } else { "" })
}

// ── Stale content eviction ────────────────────────────────────────────────────

/// After a file is edited, evict ALL stale content referencing that path from
/// the messages array. This covers:
///   - read_file results (full file content with now-wrong hashes/line numbers)
///   - edit_file post-edit echoes (±10 line excerpts with now-wrong hashes)
///   - any bash output containing rg-style path:line references to this file
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

                    // bash output containing rg-style path:line references
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
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    mcp: &McpClient,
    config: &AgentConfig,
) -> String {
    match name {
        "bash" => {
            // Graph intercept: if the command is a plain grep/rg for a known symbol,
            // return the indexed location instead of running the subprocess.
            if let Some(intercept) = bash_graph_intercept(args, config) {
                intercept
            } else {
                tools::bash::execute(args).await.unwrap_or_else(|e| format!("[Tool error: {e}]"))
            }
        }

        // ── find_symbol — in-memory graph lookup, zero disk reads
        "find_symbol" => {
            match &config.project_graph {
                Some(g) => tools::pie_tool::execute(args, g),
                None => "[find_symbol: no project graph available for this session]".to_string(),
            }
        }
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

/// If the bash command is a plain grep/rg for a symbol name that's in the
/// project graph, short-circuit with the indexed location.
///
/// Only intercepts simple identifier patterns (no regex metacharacters).
/// Falls through to real bash for complex patterns or unknown identifiers.
fn bash_graph_intercept(args: &Value, config: &AgentConfig) -> Option<String> {
    let graph = config.project_graph.as_ref()?;
    let command = args["command"].as_str()?;

    // Extract the pattern from common grep/rg invocations:
    //   grep -n "Foo" src/...    rg "Foo"    grep -rn 'Foo'
    let pattern = extract_grep_pattern(command)?;

    // Only intercept plain words — skip regex metacharacters
    let is_plain = !pattern.chars().any(|c| ".*+?[](){}\\^$|:".contains(c));
    if !is_plain || pattern.len() < 2 {
        return None;
    }

    // Check if grep targets a specific file that's already in the project index.
    // e.g. grep -n "token" src/agent.rs — model is searching a file it likely already read.
    // Run a lightweight in-memory grep against the file content and return results directly,
    // avoiding a subprocess and making the content immediately available without a round-trip.
    if let Some(target_file) = extract_grep_target_file(command) {
        if graph.file_lines.contains_key(target_file) {
            if let Ok(content) = std::fs::read_to_string(target_file) {
                let hits: Vec<String> = content.lines()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&pattern.to_lowercase()))
                    .take(30)
                    .map(|(i, line)| format!("{}:{}", i + 1, line.trim()))
                    .collect();
                if !hits.is_empty() {
                    return Some(format!(
                        "[graph intercept — searched {target_file} in-memory]\n{}\n\
                         Use read_file(path=\"{target_file}\", line_range=[N-5, N+30]) to read context.",
                        hits.join("\n")
                    ));
                }
            }
        }
    }

    // Symbol name lookup — exact match in the index
    let files = graph.by_name.get(pattern)?;
    if files.is_empty() {
        return None;
    }

    let locations: Vec<String> = files.iter().map(|f| {
        match graph.symbols.iter().find(|s| s.name == pattern && &s.file == f) {
            Some(s) if s.line > 0 => {
                let start = s.line.saturating_sub(1).max(1);
                format!(
                    "  {f}:{} ({}) → read_file(path=\"{f}\", line_range=[{start}, {}])",
                    s.line, s.kind.label(), s.end_line
                )
            }
            _ => format!("  {f}"),
        }
    }).collect();

    Some(format!(
        "[graph intercept] '{}' found in index — no grep needed:\n{}\nUse the EXACT line_range shown — it covers the complete body.",
        pattern,
        locations.join("\n")
    ))
}

/// Extract a specific file path argument from a grep command, if present.
/// e.g. `grep -n "token" src/agent.rs` → Some("src/agent.rs")
/// Returns None for recursive/wildcard searches (src/*.rs, src/).
fn extract_grep_target_file(command: &str) -> Option<&str> {
    // Last non-flag, non-pattern token that looks like a file path (contains '/')
    // or ends in a known source extension.
    let tokens: Vec<&str> = command.split_whitespace().collect();
    // Skip the command itself and quoted pattern (first non-flag after command)
    let mut saw_pattern = false;
    for tok in tokens.iter().skip(1) {
        if tok.starts_with('-') { continue; }
        if !saw_pattern {
            saw_pattern = true; // this is the pattern
            continue;
        }
        // This is a path argument — check it's a specific file, not a glob/dir
        if tok.contains('*') || tok.ends_with('/') { return None; }
        let has_src_ext = ["rs", "ts", "tsx", "js", "py", "go"].iter()
            .any(|e| tok.ends_with(&format!(".{e}")));
        if has_src_ext || tok.contains('/') {
            return Some(tok);
        }
    }
    None
}

/// Extract the grep/rg search pattern from a command string.
/// Returns None if the command isn't a simple grep/rg invocation.
fn extract_grep_pattern(command: &str) -> Option<&str> {
    let cmd = command.trim();

    // Must start with grep or rg
    if !cmd.starts_with("grep") && !cmd.starts_with("rg") {
        return None;
    }

    // Find a quoted or unquoted pattern argument.
    // Strategy: skip flags (starting with -), grab first non-flag token.
    let mut tokens = cmd.split_whitespace().skip(1); // skip "grep"/"rg"
    loop {
        let tok = tokens.next()?;
        if tok.starts_with('-') {
            // Flag — skip its argument if it takes one (e.g. -e, --include, -f)
            if matches!(tok, "-e" | "-f" | "--include" | "--exclude" | "--type" | "-t" | "-g") {
                tokens.next(); // skip the argument
            }
            continue;
        }
        // Strip surrounding quotes
        let pat = tok.trim_matches(|c| c == '"' || c == '\'');
        if !pat.is_empty() {
            return Some(pat);
        }
    }
}

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

/// Decide whether to skip the final "I'm done" model round-trip.
///
/// Returns `true` (break early) when:
/// - At least one file was mutated (edit/write/patch)
/// - No bash or ask_user call was made (those produce output the model needs to react to)
/// - No result contained an error or failure marker
///
/// Reads (read_file, project_index) are safe to skip past — the model already
/// consumed them to produce the edits. Bash always needs a follow-up turn so the model
/// can react to command output (test results, compile errors, etc.).
///
/// When `true`, the agent has verifiably completed the task and the next API call
/// would only produce a summary message, wasting the full accumulated context as input.
pub fn _should_skip_done_turn(
    mutated_files_nonempty: bool,
    any_bash_or_ask: bool,
    any_error: bool,
) -> bool {
    mutated_files_nonempty && !any_bash_or_ask && !any_error
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_user_message ────────────────────────────────────────────────────

    #[test]
    fn test_user_message_task_only() {
        let msg = build_user_message("do the thing", &[]);
        assert_eq!(msg, "do the thing");
    }

    #[test]
    fn test_user_message_with_attached_files() {
        let attached = vec!["src/foo.rs".to_string()];
        let msg = build_user_message("fix it", &attached);
        assert!(msg.contains("src/foo.rs"));
        assert!(msg.contains("read_file"));
        assert!(msg.ends_with("fix it"));
    }

    #[test]
    fn test_user_message_ordering() {
        // attached hints before task
        let attached = vec!["a.rs".to_string()];
        let msg = build_user_message("task", &attached);
        let attach_pos = msg.find("a.rs").unwrap();
        let task_pos = msg.rfind("task").unwrap();
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
            project_graph: None,
            project_narrative: None,
        }
    }

    #[test]
    fn test_system_prompt_contains_base() {
        let config = minimal_config();
        let prompt = build_system_prompt(&config, None);
        assert!(prompt.contains("PareCode"));
    }

    #[test]
    fn test_system_prompt_no_project_context() {
        // PIE context is now injected via message history, not the system prompt.
        // The system prompt should be lean — just identity + framing.
        let config = minimal_config();
        let prompt = build_system_prompt(&config, None);
        assert!(prompt.contains("PareCode"));
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
            project_graph: None,
            project_narrative: None,
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
        let (result, dispatched, had_error) = execute_one_tool_call(
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
        assert!(had_error, "parse error should signal had_error");
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

        let (result, dispatched, had_error) = execute_one_tool_call(
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
        assert!(had_error, "skipped dependent edit should signal had_error");
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

        let (result, dispatched, had_error) = execute_one_tool_call(
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
        assert!(had_error, "loop detection should signal had_error");
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

        let (result, dispatched, had_error) = execute_one_tool_call(
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
        assert!(had_error, "dry-run should signal had_error");
        let part = result.expect("dry_run returns Some");
        if let ContentPart::ToolResult { content, .. } = part {
            assert!(content.contains("dry-run"), "got: {content}");
            assert!(content.contains("bash"));
        }
    }

    // ── should_skip_done_turn ─────────────────────────────────────────────────
    // These tests are critical: a false positive (skipping when we shouldn't)
    // breaks multi-step tasks by cutting off the agent mid-workflow.
    //
    // Signature: should_skip_done_turn(mutated_files_nonempty, any_bash_or_ask, any_error)

    #[test]
    fn test_skip_done_turn_pure_edit_skips() {
        // Only edit_file calls, no bash, no errors → skip the "I'm done" round-trip
        assert!(_should_skip_done_turn(true, false, false));
    }

    #[test]
    fn test_skip_done_turn_read_then_edit_skips() {
        // read_file + edit_file → mutated=true, bash_or_ask=false, no errors → SKIP
        // This is the most common real-world pattern — must fire correctly.
        assert!(_should_skip_done_turn(true, false, false));
    }

    #[test]
    fn test_skip_done_turn_no_mutation_no_skip() {
        // No files mutated at all → must NOT skip
        assert!(!_should_skip_done_turn(false, false, false));
    }

    #[test]
    fn test_skip_done_turn_bash_only_no_skip() {
        // Bash ran (e.g. `cargo test`) but no files mutated → model needs results
        assert!(!_should_skip_done_turn(false, true, false));
    }

    #[test]
    fn test_skip_done_turn_bash_with_mutation_no_skip() {
        // edit_file + bash in same turn → bash output needs a follow-up reaction
        // Must NOT skip — model needs to see test/build output
        assert!(!_should_skip_done_turn(true, true, false));
    }

    #[test]
    fn test_skip_done_turn_ask_user_no_skip() {
        // ask_user was called → human hasn't responded yet, must not skip
        assert!(!_should_skip_done_turn(true, true, false));
    }

    #[test]
    fn test_skip_done_turn_error_no_skip() {
        // Mutation happened but build check failed → any_error=true
        // Must NOT skip — model needs to see and fix the error
        assert!(!_should_skip_done_turn(true, false, true));
    }

    #[test]
    fn test_skip_done_turn_hook_failure_no_skip() {
        // Hook exited non-zero — model needs to react to the failure
        assert!(!_should_skip_done_turn(true, false, true));
    }

    #[test]
    fn test_skip_done_turn_mutation_with_error_and_bash_no_skip() {
        // All flags set — most constrained case: must NOT skip
        assert!(!_should_skip_done_turn(true, true, true));
    }

    #[test]
    fn test_skip_done_turn_all_false_no_skip() {
        // Nothing happened — clearly don't skip
        assert!(!_should_skip_done_turn(false, false, false));
    }
}
