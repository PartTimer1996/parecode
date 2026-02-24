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
use crate::tools;
use crate::tui::UiEvent;

const MAX_TOOL_CALLS: usize = 40;

const SYSTEM_PROMPT_BASE: &str = r#"You are PareCode, a focused coding assistant. You help with software engineering tasks by using the available tools.

# Core principles
- Act decisively. When you know what to change, apply the edit immediately — do not deliberate about tool choice or re-confirm what you've already read.
- Be direct and efficient — use the minimum tool calls needed.
- Read files before editing them. After editing, verify the change compiles before declaring done.
- NEVER stop after only reading a file when the task requires modification. If you read a file, your next step must be an edit, write, or further action — not a summary of what you found.
- When a task is complete, say so clearly and stop calling tools.
- For routine actions, just do it. Use ask_user ONLY when genuinely uncertain between approaches that significantly affect the outcome.

# File mutation rules
- write_file: ONLY for creating brand-new files. Never use on existing files.
- edit_file: for modifying existing files — single-location changes, appending, inserting.
- patch_file: for changing 3+ non-adjacent locations in the same file in one go. Default to edit_file unless you clearly need patch_file.
- ONE edit per file per response. Line numbers and hashes change after every edit, so wait for the result before planning the next edit to the same file.

# edit_file reference
- Output lines are prefixed `N [hash] | content`. The 4-char hash is the anchor — pass it as anchor="a3f2" (no brackets, no line number) to avoid stale-line errors.
- edit_file returns a fresh excerpt after every successful edit. Use those hashes directly for follow-up edits — do NOT re-read the file to verify an edit you just made.
- append=true adds after the LAST LINE. Only use it when no relevant closing block exists (e.g. creating the first test module). Otherwise, match the closing brace with old_str and insert inside it.
- In plan mode, line numbers in the "Completed steps" preamble are STALE. Always use anchors from the pre-loaded file content.

# Reading files
- For large files: use read_file with symbols=true first, then line_range to fetch the section you need.
- Tool outputs are summarised in history. Use the recall tool to retrieve full output of any previous tool call.

# Verification
- After editing source files, verify the change compiles before declaring done.
- For replacement tasks (e.g. "replace X with Y"), use search to confirm no instances remain."#;

/// Build a compact project file map to inject into the system prompt.
/// Walks depth-2, ignores noise dirs, caps at 80 paths.
/// Returns None if cwd doesn't look like a code project.
fn build_project_map() -> Option<String> {
    use std::path::Path;

    // Only inject if there's a recognisable project root marker
    let markers = [
        "Cargo.toml", "package.json", "pyproject.toml", "go.mod",
        "Makefile", "CMakeLists.txt", ".parecode", "src",
    ];
    if !markers.iter().any(|m| Path::new(m).exists()) {
        return None;
    }

    const MAX_ENTRIES: usize = 80;
    const IGNORED: &[&str] = &[
        "node_modules", ".git", "target", ".next", "dist", "build",
        "__pycache__", ".venv", "venv", ".cache", "coverage", ".idea",
    ];

    let mut paths: Vec<String> = Vec::new();
    collect_paths(Path::new("."), 0, 2, IGNORED, &mut paths, MAX_ENTRIES);

    if paths.is_empty() {
        return None;
    }

    let map = paths.join("\n");
    Some(format!("\n\n# Project layout\n\n{map}"))
}

fn collect_paths(
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    ignored: &[&str],
    out: &mut Vec<String>,
    cap: usize,
) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| {
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        (is_file as u8, e.file_name())
    });

    for entry in entries {
        if out.len() >= cap {
            break;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden files/dirs and ignored dirs
        if name_str.starts_with('.') && name_str != ".parecode" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let path = entry.path();
        let display = path.to_string_lossy().trim_start_matches("./").to_string();

        if is_dir {
            if ignored.contains(&name_str.as_ref()) {
                continue;
            }
            out.push(format!("{display}/"));
            if depth < max_depth {
                collect_paths(&path, depth + 1, max_depth, ignored, out, cap);
            }
        } else {
            out.push(display);
        }
    }
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
    let mut cache = FileCache::default();
    let mut loop_detector = LoopDetector::default();

    // Build system prompt: base + optional project map + optional conventions
    let mut system_prompt = SYSTEM_PROMPT_BASE.to_string();
    if let Some(map) = build_project_map() {
        system_prompt.push_str(&map);
    }
    if let Some(conventions) = load_conventions() {
        system_prompt.push_str(&conventions);
    }
    // ── Git status injection ──────────────────────────────────────────────────
    // Inject `git status --short` so the model knows what files are uncommitted.
    // Skips silently if not in a git repo or git_context is disabled.
    if config.git_context {
        if let Some(status) = std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::git::GitRepo::open(&cwd))
            .and_then(|repo| repo.status_short().ok())
            .filter(|s| !s.trim().is_empty())
        {
            system_prompt.push_str(&format!(
                "\n\n# Git status\n\n```\n{}\n```",
                status.trim()
            ));
        }
    }
    let system_prompt = system_prompt.as_str();
    let system_tokens = crate::budget::estimate_tokens(system_prompt);

    // Build the first user message:
    //   1. Prior session context (summaries of earlier turns, if any)
    //   2. Attached file contents (files pinned via @ in TUI)
    //   3. The task itself
    let user_content = {
        let mut s = String::new();
        if let Some(ctx) = prior_context {
            s.push_str(&ctx);
        }
        if !attached.is_empty() {
            s.push_str("The following files have been attached for context:\n\n");
            for (path, content) in &attached {
                s.push_str(&format!("[{path}]\n{content}\n\n"));
            }
            s.push_str("---\n\n");
        }
        s.push_str(task);
        s
    };

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
        // Split <think>...</think> tokens into ThinkingChunk events so the TUI
        // can render model reasoning separately from the actual response text.
        let tx_clone = ui_tx.clone();
        let in_think = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let think_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let in_think_c = in_think.clone();
        let think_buf_c = think_buf.clone();
        let response = client
            .chat(system_prompt, &messages, &tools, move |chunk| {
                // Accumulate into a small lookahead buffer to handle tags split across chunks
                think_buf_c.lock().unwrap().push_str(chunk);
                loop {
                    let buf = think_buf_c.lock().unwrap().clone();
                    if in_think_c.load(std::sync::atomic::Ordering::Relaxed) {
                        // Looking for </think>
                        if let Some(pos) = buf.find("</think>") {
                            let thinking = &buf[..pos];
                            if !thinking.is_empty() {
                                let _ = tx_clone.send(UiEvent::ThinkingChunk(thinking.to_string()));
                            }
                            *think_buf_c.lock().unwrap() = buf[pos + 8..].to_string();
                            in_think_c.store(false, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            // No close tag yet — flush all but last 8 chars (tag might be split)
                            // Use floor_char_boundary so we don't slice inside a multi-byte char.
                            let keep_bytes = buf.len().saturating_sub(8);
                            let keep = buf.floor_char_boundary(keep_bytes);
                            if keep > 0 {
                                let _ = tx_clone.send(UiEvent::ThinkingChunk(buf[..keep].to_string()));
                                *think_buf_c.lock().unwrap() = buf[keep..].to_string();
                            }
                            break;
                        }
                    } else {
                        // Looking for <think>
                        if let Some(pos) = buf.find("<think>") {
                            let before = &buf[..pos];
                            if !before.is_empty() {
                                let _ = tx_clone.send(UiEvent::Chunk(before.to_string()));
                            }
                            *think_buf_c.lock().unwrap() = buf[pos + 7..].to_string();
                            in_think_c.store(true, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            // No open tag — flush all but last 7 chars.
                            // saturating_sub gives a byte offset; step back to a
                            // char boundary so multi-byte chars (e.g. em-dash) don't panic.
                            let keep_bytes = buf.len().saturating_sub(7);
                            let keep = buf.floor_char_boundary(keep_bytes);
                            if keep > 0 {
                                let _ = tx_clone.send(UiEvent::Chunk(buf[..keep].to_string()));
                                *think_buf_c.lock().unwrap() = buf[keep..].to_string();
                            }
                            break;
                        }
                    }
                }
            })
            .await?;
        // Flush any remaining buffer content
        {
            let remainder = think_buf.lock().unwrap().clone();
            if !remainder.is_empty() {
                if in_think.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = ui_tx.send(UiEvent::ThinkingChunk(remainder));
                } else {
                    let _ = ui_tx.send(UiEvent::Chunk(remainder));
                }
            }
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

        // No tool calls → done
        if response.tool_calls.is_empty() {
            break;
        }

        // ── Execute tool calls ────────────────────────────────────────────────
        // All tool calls from a single response are executed and all results
        // returned together (required by the OpenAI API spec).
        //
        // Dependency guard: if the model batches multiple mutating calls
        // targeting the same file, only the first is executed. The rest get a
        // stub result telling the model to re-plan after seeing that result.
        // This prevents speculative chaining (e.g. append then edit the
        // just-appended content with stale anchors).
        //
        // Read-only calls (read_file, search, list_files, bash) are always
        // executed regardless — they don't mutate state so batching is safe.
        let mut tool_results: Vec<ContentPart> = Vec::new();
        // Track which files have been mutated in this batch
        let mut mutated_files: std::collections::HashSet<String> = std::collections::HashSet::new();

        for tc in &response.tool_calls {
            tool_call_count += 1;

            // Extract the target path for mutation-detection (edit/write/append ops)
            let is_mutating = matches!(tc.name.as_str(), "edit_file" | "write_file" | "patch_file");
            let target_path = if is_mutating {
                serde_json::from_str::<serde_json::Value>(&tc.arguments)
                    .ok()
                    .and_then(|v| v["path"].as_str().map(|s| s.to_string()))
            } else {
                None
            };

            // Stub out dependent mutations: same file already mutated this batch
            let content = if let Some(ref path) = target_path {
                if mutated_files.contains(path) {
                    let stub = format!(
                        "[Not executed: '{}' was already modified by an earlier call in this \
                         batch. Re-plan this edit after seeing that result — use fresh line \
                         numbers and hashes from the post-edit context above.]",
                        path
                    );
                    let _ = ui_tx.send(UiEvent::ToolResult {
                        summary: format!("⚠ skipped dependent edit on {path}"),
                    });
                    tool_results.push(ContentPart::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: stub,
                    });
                    continue;
                }
            } else { () };
            let _ = content; // suppress unused warning

            let mut result_content = if loop_detector.record(&tc.name, &tc.arguments) {
                let _ = ui_tx.send(UiEvent::LoopWarning { tool_name: tc.name.clone() });
                format!(
                    "[Loop detected: {} called with identical arguments. \
                     Try a different approach or more specific arguments.]",
                    tc.name
                )
            } else {
                let raw_output = execute_tool(tc, config, &mut cache, &history, &ui_tx, &config.mcp).await;

                // Bash commands may mutate files — invalidate any cached reads
                // for paths that appear in the command string.
                if tc.name == "bash" {
                    if let Some(cmd) = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                        .ok()
                        .and_then(|v| v["command"].as_str().map(|s| s.to_string()))
                    {
                        cache.invalidate_if_mentioned(&cmd);
                    }
                }

                let (model_output, display_summary) = if config.dry_run {
                    (raw_output.clone(), raw_output.clone())
                } else {
                    let (full, display) = history.record(&tc.id, &tc.name, &raw_output);
                    (full, display)
                };

                let _ = ui_tx.send(UiEvent::ToolResult { summary: display_summary });

                if config.verbose {
                    let extra: Vec<&str> = model_output.lines().skip(1).take(4).collect();
                    for line in extra {
                        let _ = ui_tx.send(UiEvent::ToolResult { summary: format!("  {line}") });
                    }
                }

                model_output
            };

            // ── on_edit hooks ─────────────────────────────────────────────────
            // Run after each successful mutating call. Output is appended
            // directly into the tool result so the model sees compile/lint
            // errors and can self-correct immediately.
            if is_mutating && config.hooks_enabled && !config.hooks.on_edit.is_empty() {
                for cmd in &config.hooks.on_edit {
                    let hr = hooks::run_hook(cmd).await;
                    let success = hr.exit_code == 0;
                    let hook_line = if success && hr.output.trim().is_empty() {
                        format!("\n\n⚙ `{cmd}` ✓")
                    } else {
                        format!("\n\n⚙ `{cmd}` (exit {}):\n{}", hr.exit_code, hr.output)
                    };
                    result_content.push_str(&hook_line);
                    let _ = ui_tx.send(UiEvent::HookOutput {
                        event: "on_edit".to_string(),
                        output: hr.output,
                        exit_code: hr.exit_code,
                    });
                }
            }

            // Record mutation after successful execution and compress stale reads
            if let Some(path) = target_path {
                // Compress old read_file results for this path — their hashes
                // and line numbers are now stale, wasting context budget.
                if !result_content.contains("[Tool error") {
                    history.compress_reads_for(&path);
                    // Evict ALL stale content for this path from the messages
                    // array — reads, old edit echoes, search results. Without
                    // this, stale hashes/line numbers stay in context and
                    // actively mislead the model on subsequent edits.
                    evict_stale_content(&mut messages, &path);
                }
                mutated_files.insert(path);
            }

            tool_results.push(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: result_content,
            });
        }

        messages.push(Message {
            role: "tool".to_string(),
            content: MessageContent::Parts(tool_results),
            tool_calls: vec![],
        });

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
    if config.hooks_enabled && !config.hooks.on_task_done.is_empty() {
        for cmd in &config.hooks.on_task_done {
            let hr = hooks::run_hook(cmd).await;
            let _ = ui_tx.send(UiEvent::HookOutput {
                event: "on_task_done".to_string(),
                output: hr.output,
                exit_code: hr.exit_code,
            });
        }
    }

    // ── Git post-task ─────────────────────────────────────────────────────────
    // Emit a diff notification and optionally auto-commit.
    if config.git_context {
        if let Some(cwd) = std::env::current_dir().ok() {
            if let Some(repo) = crate::git::GitRepo::open(&cwd) {
                let ref_pt = checkpoint_hash.as_deref().unwrap_or("HEAD");
                if let Ok(stat) = repo.diff_stat_from(ref_pt) {
                    if !stat.trim().is_empty() {
                        // Count lines that describe a changed file (contain '|')
                        let files_changed = stat.lines().filter(|l| l.contains('|')).count();
                        let _ = ui_tx.send(UiEvent::GitChanges {
                            stat: stat.trim().to_string(),
                            checkpoint_hash: checkpoint_hash.clone(),
                            files_changed,
                        });
                    }
                }
                if config.auto_commit {
                    let summary: String = task
                        .lines()
                        .next()
                        .unwrap_or(task)
                        .chars()
                        .take(72)
                        .collect();
                    let msg = format!("{}{}", config.auto_commit_prefix, summary);
                    match repo.auto_commit(&msg) {
                        Ok(()) => {
                            let _ = ui_tx.send(UiEvent::GitAutoCommit { message: msg });
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::GitError(format!("auto-commit: {e}")));
                        }
                    }
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
        let mut cache = FileCache::default();
        let history = History::default();
        let raw = execute_tool(tc, config, &mut cache, &history, &ui_tx, &config.mcp).await;
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

// ── Tool execution ────────────────────────────────────────────────────────────

async fn execute_tool(
    tc: &ToolCall,
    config: &AgentConfig,
    cache: &mut FileCache,
    history: &History,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    mcp: &McpClient,
) -> String {
    let args: Value = match serde_json::from_str(&tc.arguments) {
        Ok(v) => v,
        Err(e) => return format!("[Error parsing tool arguments: {e}]"),
    };

    // Display the tool call
    let _ = ui_tx.send(UiEvent::ToolCall {
        name: tc.name.clone(),
        args_summary: format_args_summary(&args),
    });

    if config.dry_run {
        return format!("[dry-run: {} not executed]", tc.name);
    }

    match tc.name.as_str() {
        "bash" => {
            match tools::bash::execute(&args).await {
                Ok(output) => output,
                Err(e) => format!("[Tool error: {e}]"),
            }
        }
        "recall" => {
            // Try by tool_call_id first, then by tool_name
            let by_id = args["tool_call_id"].as_str()
                .and_then(|id| history.recall(id));
            if let Some(full) = by_id {
                return full.to_string();
            }
            let by_name = args["tool_name"].as_str()
                .and_then(|name| history.recall_by_name(name));
            if let Some(full) = by_name {
                return full.to_string();
            }
            "[recall: no matching tool result found]".to_string()
        }
        "ask_user" => {
            let question = args["question"].as_str().unwrap_or("(no question provided)").to_string();
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
            let _ = ui_tx.send(UiEvent::AskUser { question, reply_tx });
            match reply_rx.await {
                Ok(answer) => answer,
                Err(_) => "[ask_user: no reply received (channel closed)]".to_string(),
            }
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let is_symbols = args["symbols"].as_bool().unwrap_or(false);
            let has_range = !args["line_range"].is_null();

            // Cache only serves/stores full-content reads (no line_range, no symbols).
            // Symbol-index reads are navigation-only and must not be cached as content.
            if !has_range && !is_symbols {
                if let Some(hit) = cache.check(path) {
                    let _ = ui_tx.send(UiEvent::CacheHit { path: path.to_string() });
                    return hit.into_message();
                }
            }

            match tools::dispatch("read_file", &args) {
                Ok(output) => {
                    // Only cache full-content reads
                    if !has_range && !is_symbols {
                        cache.store(path, output.clone());
                    }
                    output
                }
                Err(e) => format!("[Tool error: {e}]"),
            }
        }
        "write_file" | "edit_file" | "patch_file" => {
            let path = args["path"].as_str().unwrap_or("");
            match tools::dispatch(&tc.name, &args) {
                Ok(o) => {
                    cache.invalidate(path);
                    o
                }
                Err(e) => {
                    // On edit failure, include the actual file content so the model
                    // can see exactly what's there and correct its old_str.
                    let hint = if let Some(hit) = cache.check(path) {
                        format!(
                            "\n\nCurrent file content for reference:\n{}",
                            hit.content
                        )
                    } else if let Ok(content) = std::fs::read_to_string(path) {
                        format!("\n\nCurrent file content for reference:\n{content}")
                    } else {
                        String::new()
                    };
                    format!("[Tool error: {e}]{hint}")
                }
            }
        }
        _ => {
            // Try native tools first, then fall through to MCP
            if tools::is_native(&tc.name) {
                match tools::dispatch(&tc.name, &args) {
                    Ok(output) => output,
                    Err(e) => format!("[Tool error: {e}]"),
                }
            } else {
                // MCP tool: qualified name contains a '.'
                match mcp.call(&tc.name, args).await {
                    Ok(output) => output,
                    Err(e) => format!("[MCP tool error: {e}]"),
                }
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
