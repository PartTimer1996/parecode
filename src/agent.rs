use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::budget::{Budget, LoopDetector};
use crate::cache::FileCache;
use crate::client::{Client, ContentPart, Message, MessageContent, Tool, ToolCall};
use crate::history::History;
use crate::mcp::McpClient;
use crate::tools;
use crate::tui::UiEvent;

const MAX_TOOL_CALLS: usize = 40;

const SYSTEM_PROMPT_BASE: &str = r#"You are Forge, a focused coding assistant. You help with software engineering tasks by using the available tools.

Guidelines:
- Be direct and efficient — use the minimum tool calls needed
- Read files before editing them
- Prefer edit_file over write_file for existing files (preserves unchanged content)
- After editing source files, verify the change compiles before declaring done
- For replacement tasks (e.g. "replace X with Y"), use search to confirm no instances of X remain before declaring done
- When a task is complete, say so clearly and stop calling tools
- Do not re-read files you have already read in this session
- For large files: use read_file with symbols=true to get a function/class index first, then read_file with line_range=[start,end] to fetch only the section you need
- Tool outputs are summarised in history to save context. Use the recall tool to retrieve the full output of any previous tool call when you need it.
- Do not ask for permission mid-task. If something is clearly required (adding a dependency, creating a file, running a command), do it and report what you did. Only stop to ask if there are genuinely multiple valid approaches that change the outcome significantly."#;

/// Build a compact project file map to inject into the system prompt.
/// Walks depth-2, ignores noise dirs, caps at 80 paths.
/// Returns None if cwd doesn't look like a code project.
fn build_project_map() -> Option<String> {
    use std::path::Path;

    // Only inject if there's a recognisable project root marker
    let markers = [
        "Cargo.toml", "package.json", "pyproject.toml", "go.mod",
        "Makefile", "CMakeLists.txt", ".forge", "src",
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
        if name_str.starts_with('.') && name_str != ".forge" {
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

/// Load project conventions from AGENTS.md, CLAUDE.md, or .forge/conventions.md.
/// Returns None if no conventions file is found.
fn load_conventions() -> Option<String> {
    let candidates = [
        "AGENTS.md",
        "CLAUDE.md",
        ".forge/conventions.md",
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
    pub profile_name: String,
    pub model: String,
    pub show_timestamps: bool,
    pub mcp: Arc<McpClient>,
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
    // Merge native tools + MCP-discovered tools into one list for the model
    let mut tools = tools::all_definitions();
    let mcp_tools = config.mcp.all_tools().await;
    for mt in &mcp_tools {
        tools.push(Tool {
            name: mt.qualified_name.clone(),
            description: mt.description.clone(),
            parameters: mt.input_schema.clone(),
        });
    }
    let mut messages: Vec<Message> = Vec::new();
    let mut total_input_tokens = 0u32;
    let mut total_output_tokens = 0u32;
    let mut tool_call_count = 0usize;

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

    messages.push(Message {
        role: "user".to_string(),
        content: MessageContent::from(user_content),
    });

    loop {
        cache.next_turn();

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

        // ── Call the model ────────────────────────────────────────────────────
        let tx_clone = ui_tx.clone();
        let response = client
            .chat(system_prompt, &messages, &tools, move |chunk| {
                let _ = tx_clone.send(UiEvent::Chunk(chunk.to_string()));
            })
            .await?;

        total_input_tokens += response.input_tokens;
        total_output_tokens += response.output_tokens;

        if config.verbose && (response.input_tokens > 0 || response.output_tokens > 0) {
            let _ = ui_tx.send(UiEvent::TokenStats {
                input: response.input_tokens,
                output: response.output_tokens,
                total_input: total_input_tokens,
                total_output: total_output_tokens,
            });
        }

        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::from(response.text.clone()),
        });

        // No tool calls → done
        if response.tool_calls.is_empty() {
            break;
        }

        // ── Execute tool calls ────────────────────────────────────────────────
        let mut tool_results: Vec<ContentPart> = Vec::new();

        for tc in &response.tool_calls {
            tool_call_count += 1;

            // Loop detection
            if loop_detector.record(&tc.name, &tc.arguments) {
                let _ = ui_tx.send(UiEvent::LoopWarning { tool_name: tc.name.clone() });
                let msg = format!(
                    "[Loop detected: {} called with identical arguments. \
                     Try a different approach or more specific arguments.]",
                    tc.name
                );
                tool_results.push(ContentPart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content: msg,
                });
                continue;
            }

            let mut raw_output = execute_tool(tc, config, &mut cache, &history, &ui_tx, &config.mcp).await;

            // Auto build-check after successful file edits
            if !config.dry_run && is_file_mutating(&tc.name, &raw_output) {
                if let Some(check) = run_build_check().await {
                    let _ = ui_tx.send(UiEvent::ToolResult { summary: check.clone() });
                    raw_output.push_str(&format!("\n\n[auto build check]\n{check}"));
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

            tool_results.push(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: model_output,
            });
        }

        messages.push(Message {
            role: "tool".to_string(),
            content: MessageContent::Parts(tool_results),
        });
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
    });

    Ok(())
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
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");

            // Cache hit
            if args["line_range"].is_null() {
                if let Some(hit) = cache.check(path) {
                    let _ = ui_tx.send(UiEvent::CacheHit { path: path.to_string() });
                    return hit.into_message();
                }
            }

            match tools::dispatch("read_file", &args) {
                Ok(output) => {
                    if args["line_range"].is_null() {
                        cache.store(path, output.clone());
                    }
                    output
                }
                Err(e) => format!("[Tool error: {e}]"),
            }
        }
        "write_file" | "edit_file" => {
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

/// Returns true if this tool call successfully mutated source files.
fn is_file_mutating(tool_name: &str, output: &str) -> bool {
    match tool_name {
        "write_file" | "edit_file" => output.starts_with('✓'),
        "bash" => {
            // Bash mutations: sed -i, mv, cp touching source files, etc.
            // Heuristic: output has no error AND command looks file-mutating.
            // We can't introspect the command here, so we rely on the build check
            // being cheap enough to always run after bash too.
            false // only trigger on explicit file tools to avoid noise
        }
        _ => false,
    }
}

/// Detect the project's build system and run a fast syntax/type check.
/// Returns None if no known build system found.
async fn run_build_check() -> Option<String> {
    use std::path::Path;
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    // Detect build system by marker files
    let (cmd, args): (&str, &[&str]) = if Path::new("Cargo.toml").exists() {
        ("cargo", &["check", "--message-format=short", "-q"])
    } else if Path::new("tsconfig.json").exists() {
        ("npx", &["tsc", "--noEmit", "--pretty", "false"])
    } else if Path::new("package.json").exists() {
        ("npm", &["run", "build", "--if-present"])
    } else if Path::new("pyproject.toml").exists() || Path::new("setup.py").exists() {
        ("python", &["-m", "py_compile"])
    } else {
        return None;
    };

    let fut = Command::new(cmd).args(args).output();
    let output = match timeout(Duration::from_secs(30), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(_)) | Err(_) => return None,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.is_empty() {
        stdout.trim().to_string()
    } else if stdout.is_empty() {
        stderr.trim().to_string()
    } else {
        format!("{}\n{}", stdout.trim(), stderr.trim())
    };

    if output.status.success() {
        Some("✓ build check passed. Now use search to verify the task goal is fully met before declaring done.".to_string())
    } else {
        // Return errors, capped at 40 lines
        let lines: Vec<&str> = combined.lines().collect();
        let shown = lines.len().min(40);
        let mut result = lines[..shown].join("\n");
        if lines.len() > shown {
            result.push_str(&format!("\n[+{} lines truncated]", lines.len() - shown));
        }
        Some(format!("✗ build check failed:\n{result}"))
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
