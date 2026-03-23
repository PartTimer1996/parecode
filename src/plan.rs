/// Plan/Execute architecture for PareCode — Phase 4.
///
/// A Plan is a `Vec<PlanStep>` owned by the scaffold. Each step executes with a
/// fresh context containing only its specific files. The model never sees history
/// from previous steps — only its current bounded instruction.
///
/// Flow:
///   1. User types `/plan "task"`
///   2. PareCode calls the model once to generate a structured plan (JSON)
///   3. TUI shows plan review overlay — user can annotate steps or approve
///   4. Each step runs as an isolated agent call (fresh context, step files only)
///   5. After each step, verification runs; failure surfaces for user decision
///
/// Plans are persisted to `.parecode/plans/{timestamp}.json` so they can be resumed.
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use tokio::sync::mpsc;

use crate::client::{Client, ContentPart, Message, MessageContent, Tool, ToolCall};
use crate::pie::ProjectGraph;
use crate::tui::UiEvent;

// ── Core data structures ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PlanStatus {
    Pending,
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StepStatus {
    Pending,   // not yet reviewed
    Approved,  // user reviewed and accepted — awaiting execution
    Running,   // currently executing
    Pass,      // executed and verified successfully
    Fail,      // executed but failed verification
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Verification {
    None,
    /// Check a file was modified after the step
    FileChanged(String),
    /// Check a pattern no longer appears in a file
    PatternAbsent { file: String, pattern: String },
    /// Run a shell command and check exit code 0
    CommandSuccess(String),
    /// Placeholder: step verification passes by default. Use CommandSuccess for actual build checks.
    BuildSuccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    /// Human-readable description shown in the TUI review panel
    pub description: String,
    /// Model-facing instruction (what the model is told to do for this step)
    pub instruction: String,
    /// Files to load as context — ONLY these, nothing else
    pub files: Vec<String>,
    /// How to verify the step succeeded
    pub verify: Verification,
    /// Execution status
    pub status: StepStatus,
    /// Max tool calls for this step (default: 8)
    pub tool_budget: usize,
    /// Optional user annotation — appended to instruction as "\n\nUser note: {}"
    pub user_annotation: Option<String>,
    /// Filled in after the step completes — brief summary of what was done.
    /// Injected into subsequent steps so they know what changed.
    #[serde(default)]
    pub completed_summary: Option<String>,
}

impl PlanStep {
    /// Build the full instruction the model sees: base + user annotation
    pub fn effective_instruction(&self) -> String {
        match &self.user_annotation {
            Some(note) if !note.trim().is_empty() => {
                format!("{}\n\nUser note: {}", self.instruction, note.trim())
            }
            _ => self.instruction.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub task: String,
    pub steps: Vec<PlanStep>,
    pub current: usize,
    pub status: PlanStatus,
    /// Unix timestamp when the plan was created
    pub created_at: i64,
    /// cwd basename at plan creation time
    pub project: String,
}

impl Plan {
    pub fn new(task: String, steps: Vec<PlanStep>, project: String) -> Self {
        Self {
            task,
            steps,
            current: 0,
            status: PlanStatus::Pending,
            created_at: chrono::Utc::now().timestamp(),
            project,
        }
    }

    /// Estimate token cost for this plan.
    /// Heuristic: base_tokens(500) + instruction_len/4 + file_sizes/4 per step,
    /// summed and multiplied by 1.3 to account for tool results and model responses.
    /// Returns (low_estimate, high_estimate) in tokens.
    pub fn estimate_tokens(&self) -> (usize, usize) {
        let base_per_step = 500usize;
        let overhead_factor_low = 10;  // ×1.0 — minimum
        let overhead_factor_high = 13; // ×1.3 — with tool results + model responses

        let raw: usize = self.steps.iter().map(|step| {
            let instruction_tokens = step.instruction.chars().count() / 4;
            let file_tokens: usize = step.files.iter().map(|f| {
                // Estimate file size from disk — fall back to 1000 tokens if unreadable
                std::fs::read_to_string(f)
                    .map(|c| c.chars().count() / 4)
                    .unwrap_or(1000)
            }).sum();
            base_per_step + instruction_tokens + file_tokens
        }).sum();

        let low = raw * overhead_factor_low / 10;
        let high = raw * overhead_factor_high / 10;
        (low, high)
    }

    /// Format the cost estimate as a compact string for display.
    /// e.g. "est. 8k–12k tokens"
    pub fn estimate_display(&self, cost_per_mtok: Option<f64>) -> String {
        let (low, high) = self.estimate_tokens();

        fn fmt_k(n: usize) -> String {
            if n >= 1000 { format!("{}k", n / 1000) } else { n.to_string() }
        }

        let token_str = format!("est. {}–{} tokens", fmt_k(low), fmt_k(high));

        if let Some(rate) = cost_per_mtok {
            let usd_low  = (low  as f64 / 1_000_000.0) * rate;
            let usd_high = (high as f64 / 1_000_000.0) * rate;
            if usd_high < 0.01 {
                format!("{token_str}  ·  <$0.01")
            } else {
                format!("{token_str}  ·  ~${:.2}–${:.2}", usd_low, usd_high)
            }
        } else {
            token_str
        }
    }
}

// ── Plan persistence ──────────────────────────────────────────────────────────

/// Directory for saved plans: `.parecode/plans/` relative to cwd.
pub fn plans_dir() -> PathBuf {
    PathBuf::from(".parecode/plans")
}

pub fn save_plan(plan: &Plan) -> Result<PathBuf> {
    let dir = plans_dir();
    std::fs::create_dir_all(&dir)?;
    let filename = format!("{}-plan.json", plan.created_at);
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(plan)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

// ── Plan generation ───────────────────────────────────────────────────────────

/// Single system prompt covering both exploration and output.
/// No phase switch — the model explores with tools, then outputs JSON when done.
/// Keeping one consistent prompt avoids models getting confused by a system-prompt
/// change mid-conversation (the root cause of XML tool call fallback on phase 2).
const PLANNER_PROMPT: &str = r#"You are PareCode, a coding assistant. Your job is to produce a structured edit plan — NOT to understand or explain the codebase.

ORIENTATION: The project index and context file symbols are pre-loaded in your context. You already have file names, cluster membership, and (for attached files) exact symbol line numbers. Use them.

TOOL ORDER — follow this sequence, never skip steps:
1. orient(query="task keywords") — ONE call returns all task-relevant structs with signatures,
   locations, and call connections in pipeline order. Use this first, always.
2. check_wiring(field="X") — REQUIRED whenever you find a field: verifies full pipeline.
   WITHOUT list = your plan steps. Call this before concluding any propagation is complete.
3. read_file(path, line_range=[N,M]) — see READ BUDGET below.

READ BUDGET — strictly enforced:
- You have at most 3 reads total. Spend them only on function bodies with imperative logic
  you must quote in a step instruction (e.g. a constructor, a renderer, an event handler).
- Struct/enum signatures from orient are authoritative — reading a struct wastes your budget.
- Do NOT re-read a range you already read. Do NOT read speculatively.
- Always provide line_range (you have line numbers from orient and known locations).
  Full-file reads are BLOCKED.

EXPLORATION RULES — STRICTLY ENFORCED:
- If a symbol is listed in "Context file symbols" above, you already have its line number — do NOT call orient for it.
- Finding a field in ONE struct is not enough — always call check_wiring to verify the full chain.
- check_wiring's WITHOUT list = your plan steps. Do not generate steps for structs already in the WITH list.
- Maximum 8 tool calls total. Stop as soon as you have file:line refs for everything the plan touches.

OUTPUT: When you have all locations, immediately output the JSON plan (no markdown fences, no prose):

{
  "steps": [
    {
      "description": "human-readable one-liner shown to user",
      "instruction": "precise instruction with EVERY file:line ref — never say 'find X' or 'locate Y'",
      "files": ["src/foo.rs", "src/bar.rs"],
      "verify": "none",
      "tool_budget": 15
    }
  ]
}

STEP RULES:
- Each step runs in TOTAL ISOLATION — the executor sees ONLY files listed in "files"
- List EVERY file the step needs to read OR modify (max 10 per step)
- Every instruction MUST contain exact file paths and line numbers
- Prefer 4–8 steps; do not split naturally-coupled changes into micro-steps
- verify: "none" | "command:CMD" | "changed:FILE" | "absent:FILE:PATTERN""#;

/// Response from the model during plan generation.
#[derive(Debug, Deserialize)]
struct PlanResponse {
    steps: Vec<PlanStepRaw>,
}

#[derive(Debug, Deserialize)]
struct PlanStepRaw {
    description: String,
    instruction: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default = "default_verify")]
    verify: String,
    #[serde(default = "default_tool_budget")]
    tool_budget: usize,
}

fn default_verify() -> String { "none".to_string() }
fn default_tool_budget() -> usize { 15 }

fn parse_verification(s: &str) -> Verification {
    if s == "none" || s.is_empty() {
        return Verification::None;
    }
    if s == "build" {
        return Verification::BuildSuccess;
    }
    if let Some(rest) = s.strip_prefix("command:") {
        return Verification::CommandSuccess(rest.to_string());
    }
    if let Some(rest) = s.strip_prefix("changed:") {
        return Verification::FileChanged(rest.to_string());
    }
    if let Some(rest) = s.strip_prefix("absent:") {
        // "absent:file.ts:pattern"
        let mut parts = rest.splitn(2, ':');
        let file = parts.next().unwrap_or("").to_string();
        let pattern = parts.next().unwrap_or("").to_string();
        return Verification::PatternAbsent { file, pattern };
    }
    Verification::None
}

// ── Planner tools ─────────────────────────────────────────────────────────────

/// Exploration tools for the planner — orient + check_wiring + read_file from the shared registry.
/// Same tools the agent uses, so the model gets the same enriched definitions.
fn planner_tools() -> Vec<Tool> {
    [crate::tools::TOOL_ORIENT, crate::tools::TOOL_CHECK_WIRING, crate::tools::TOOL_READ_FILE]
        .iter()
        .filter_map(|name| crate::tools::get_tool(name))
        .map(|v| Tool {
            name: v["name"].as_str().unwrap_or("").to_string(),
            description: v["description"].as_str().unwrap_or("").to_string(),
            parameters: v["parameters"].clone(),
        })
        .collect()
}

fn execute_planner_tool(call: &ToolCall, graph: &ProjectGraph) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    match call.name.as_str() {
        "orient" => crate::tools::pie_tool::orient_execute(&args, graph),
        "find_symbol" => crate::tools::pie_tool::execute(&args, graph),
        "trace_calls" => crate::tools::pie_tool::trace_calls_execute(&args, graph),
        "check_wiring" => crate::tools::pie_tool::check_wiring_execute(&args, graph),
        "read_file" => {
            // Require line_range — a ranged read means the model knows what it wants,
            // which means it has already oriented via orient/check_wiring/known_locations.
            // Full-file reads in the planner are exploratory and wasteful; block them.
            let has_range = args["line_range"].as_array()
                .map_or(false, |a| !a.is_empty());
            if !has_range {
                return format!(
                    "[planner: full-file reads are blocked — you already have line numbers \
                     from orient and the known locations. \
                     Call read_file with line_range=[start, end] for the specific section you need.]"
                );
            }
            crate::tools::pie_tool::smart_read(&args, graph)
        }
        other => format!("[unknown planner tool: {other}]"),
    }
}

// ── Plan generation ───────────────────────────────────────────────────────────


/// Append `s` to `buf` and write the whole buffer to `.parecode/last_plan_session.txt`.
/// Best-effort — silently ignored on I/O error. Updated after every turn and tool call
/// so the file is readable mid-run (`tail -f .parecode/last_plan_session.txt`).
fn log_append(buf: &mut String, s: &str) {
    buf.push_str(s);
    let _ = std::fs::write(".parecode/last_plan_session.txt", buf.as_str());
}

/// Write the full planner prompt to `.parecode/last_plan_prompt.txt` for inspection.
/// Best-effort — silently ignored on any I/O error.
fn dump_plan_prompt(system_prompt: &str, messages: &[Message]) {
    use std::fmt::Write as FmtWrite;
    let mut out = String::new();
    let _ = writeln!(out, "=== SYSTEM PROMPT ===\n{system_prompt}\n");
    for (i, msg) in messages.iter().enumerate() {
        let content_str = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    ContentPart::ToolResult { tool_use_id, content } => {
                        format!("[ToolResult id={tool_use_id}]\n{content}")
                    }
                    _ => format!("{p:?}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let _ = writeln!(out, "\n--- MESSAGE {i} ({}) ---\n{content_str}", msg.role);
    }
    let _ = std::fs::write(".parecode/last_plan_prompt.txt", out);
}



/// Best-effort symbol-enrichment pass: for any symbol name that appears in a
/// step instruction but has no line number referenced, append an index hint.
/// This ensures executors always have a concrete starting point without needing
/// to call list_symbols themselves.
fn enrich_step_instructions(steps: &mut Vec<PlanStep>, graph: &ProjectGraph) {
    for step in steps.iter_mut() {
        for sym in graph.symbols.iter() {
            // Only enrich if the instruction mentions the symbol name but not its line
            if step.instruction.contains(&sym.name)
                && !step.instruction.contains(&format!("line {}", sym.line))
            {
                step.instruction.push_str(&format!(
                    "\n[index: `{}` at line {} in {}]",
                    sym.name, sym.line, sym.file
                ));
            }
        }
    }
}

/// Call the model to generate a plan for `task`.
///
/// Unified loop: one system prompt, model explores freely with find_symbol + read_file,
/// then outputs JSON when done. No phase switch — avoids model confusion from a
/// mid-conversation system-prompt change (the root cause of XML tool call fallback).
///
/// When the model stops calling tools:
///   - If its output is parseable JSON → that IS the plan, done.
///   - If it's prose (exploration notes) → push "now output JSON" and loop once more
///     with no tools so the model can commit.
pub async fn generate_plan(
    task: &str,
    client: &Client,
    project: &str,
    context_files: &[String],
    graph: &ProjectGraph,
    narrative: Option<&crate::narrative::ProjectNarrative>,
    flow_paths: Option<&crate::flowpaths::FlowPathIndex>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    on_chunk: impl Fn(&str) + Send + Sync + 'static,
) -> Result<Plan> {
    let task_start = std::time::Instant::now();
    let mut total_input: u32 = 0;
    let mut total_output: u32 = 0;
    let mut tool_call_count: usize = 0;

    // ── Session log ───────────────────────────────────────────────────────────
    // Accumulated in a String; written to .parecode/last_plan_session.txt after
    // every turn and tool call so `tail -f` works during a live run.
    let _ = std::fs::create_dir_all(".parecode");
    let mut session_log = format!("=== PLAN SESSION ===\nTask: {task}\n\n");

    // ── PIE context assembly — same path as run_tui ───────────────────────────
    let pie_ctx = crate::pie::build_pie_context(task, context_files, graph, narrative, flow_paths);

    let mut messages: Vec<Message> = Vec::new();
    messages.extend(pie_ctx.injection_messages);

    // Task message — symbol locations immediately before the task for maximum salience.
    let focus_files = pie_ctx.focus_files;
    let mut task_content = pie_ctx.user_prefix;
    task_content.push_str(&format!("Task: {task}"));
    messages.push(Message {
        role: "user".to_string(),
        content: MessageContent::Text(task_content),
        tool_calls: vec![],
    });

    // ── Debug prompt dump ─────────────────────────────────────────────────────
    // Write the full prompt to .parecode/last_plan_prompt.txt for tuning/inspection.
    dump_plan_prompt(PLANNER_PROMPT, &messages);

    // ── Unified exploration + output loop ─────────────────────────────────────
    let tools = planner_tools();
    const MAX_TURNS: usize = 20;
    const TOOL_CAP: usize = 8; // hard cap — matches the prompt's stated budget
    let mut output_mode = false; // true = no tools, model must output JSON this turn
    let mut json_text_raw = String::new();

    for turn_idx in 0..MAX_TURNS {
        // Enforce hard tool cap: once hit, strip tools and demand JSON regardless of model intent
        if tool_call_count >= TOOL_CAP && !output_mode {
            messages.push(Message {
                role: "user".to_string(),
                content: MessageContent::Text(format!(
                    "Tool budget reached ({TOOL_CAP}/{TOOL_CAP}). Output the JSON plan now using what \
                     you have found. Approximate locations are fine — the executor reads full \
                     context. Do NOT explore further."
                )),
                tool_calls: vec![],
            });
            output_mode = true;
        }
        let current_tools: &[Tool] = if output_mode { &[] } else { &tools };

        log_append(
            &mut session_log,
            &format!(
                "\n╔═══ TURN {} {} ═══╗\n",
                turn_idx + 1,
                if output_mode { "(output — no tools)" } else { "" }
            ),
        );

        let resp = client
            .chat(PLANNER_PROMPT, &messages, current_tools, &on_chunk)
            .await?;

        // Append full model response text (includes <think> blocks) to log
        log_append(&mut session_log, &resp.text);

        total_input += resp.input_tokens;
        total_output += resp.output_tokens;

        if resp.input_tokens > 0 || resp.output_tokens > 0 {
            let _ = ui_tx.send(UiEvent::TokenStats {
                _input: resp.input_tokens,
                _output: resp.output_tokens,
                total_input,
                total_output,
                tool_calls: tool_call_count,
            });
        }

        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::Text(resp.text.clone()),
            tool_calls: resp.tool_calls.clone(),
        });

        if resp.tool_calls.is_empty() {
            // XML tool call syntax (some models use proprietary formats)
            if resp.text.contains("<invoke") && resp.text.contains("</invoke>") {
                messages.push(Message {
                    role: "user".to_string(),
                    content: MessageContent::Text(
                        "Use the JSON tool_calls format — do not emit XML. \
                         Call find_symbol or read_file using the API tool_calls mechanism."
                            .to_string(),
                    ),
                    tool_calls: vec![],
                });
                continue;
            }

            // Try to parse as JSON plan — if it works, we're done
            if extract_json(&resp.text).is_some() {
                json_text_raw = resp.text.clone();
                log_append(&mut session_log, &format!("\n\n=== FINAL PLAN ===\n{}\n", resp.text));
                break;
            }

            if output_mode {
                // Already asked for JSON and still didn't get it
                return Err(anyhow::anyhow!(
                    "Plan parse error: model was asked to output JSON but did not.\n\nModel response:\n{}",
                    resp.text
                ));
            }

            // Model output prose (exploration notes) — ask for JSON now
            messages.push(Message {
                role: "user".to_string(),
                content: MessageContent::Text(
                    "You have finished exploring. Now output the JSON plan.".to_string(),
                ),
                tool_calls: vec![],
            });
            output_mode = true;
            continue;
        }

        // Execute tool calls — emit ToolCall/ToolResult events for TUI visibility
        let mut tool_results: Vec<ContentPart> = Vec::new();
        for tc in &resp.tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
            let args_summary = match tc.name.as_str() {
                "find_symbol" => args["name"].as_str().unwrap_or("?").to_string(),
                "read_file" => {
                    let path = args["path"].as_str().unwrap_or("?");
                    if let (Some(s), Some(e)) = (args["line_range"].get(0), args["line_range"].get(1)) {
                        format!("{path} [{s}–{e}]")
                    } else {
                        path.to_string()
                    }
                }
                _ => tc.arguments.chars().take(40).collect(),
            };
            let _ = ui_tx.send(UiEvent::ToolCall {
                name: tc.name.clone(),
                args_summary: args_summary.clone(),
            });

            log_append(&mut session_log, &format!("\n\n◆ {} {}\n", tc.name, args_summary));

            let result = execute_planner_tool(tc, graph);
            let result_summary: String = result.lines().next().unwrap_or("").chars().take(80).collect();
            let _ = ui_tx.send(UiEvent::ToolResult { summary: result_summary });

            log_append(&mut session_log, &format!("→ {result}\n"));

            tool_results.push(ContentPart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: result,
            });
            tool_call_count += 1;
        }

        let _ = ui_tx.send(UiEvent::TokenStats {
            _input: 0,
            _output: 0,
            total_input,
            total_output,
            tool_calls: tool_call_count,
        });

        // Prepend a compact reminder so the model never re-calls find_symbol for
        // symbols it already had at turn 0 — context decay kills salience by turn 5+.
        let reminder = crate::pie::build_known_locations_reminder(&focus_files, graph);
        let mut parts: Vec<ContentPart> = Vec::new();
        if !reminder.is_empty() {
            parts.push(ContentPart::Text { text: reminder });
        }
        parts.extend(tool_results);
        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Parts(parts),
            tool_calls: vec![],
        });
    }

    if json_text_raw.is_empty() {
        return Err(anyhow::anyhow!(
            "Plan generation exhausted {MAX_TURNS} turns without producing a plan. \
             Try simplifying the task or using a different model."
        ));
    }

    let elapsed = task_start.elapsed().as_secs() as u32;
    let _ = ui_tx.send(UiEvent::SystemMsg(format!(
        "⚙ plan generated — {tool_call_count} explore calls · {}→+{} tokens · {elapsed}s",
        total_input, total_output
    )));

    // ── Parse JSON response ────────────────────────────────────────────────────
    let json_text_raw = json_text_raw.trim();

    // Strip markdown fences if the model wrapped it despite instructions
    // Handle ```json, ```json <whitespace>, ```, etc. — strip from start then end
    let json_text = extract_json(json_text_raw)
        .ok_or_else(|| anyhow::anyhow!("Plan parse error: could not extract JSON from model response.\n\nModel response:\n{json_text_raw}"))?
        .to_string();

    // Defensive: reject empty/whitespace-only responses before JSON parsing
    // Small models (Qwen3 etc.) frequently emit literal \n/\t/\r inside JSON
    // strings, which is invalid JSON and causes serde_json to reject the whole plan.
    // We walk char-by-char, tracking whether we're inside a string, and replace
    // bare control chars with their JSON escape sequences.
    let json_text = sanitize_json_strings(&json_text);

    // Defensive: reject empty/whitespace-only responses before JSON parsing
    // This prevents confusing "expected value at line 1 column 1" errors
    if json_text.is_empty() {
        return Err(anyhow::anyhow!(
            "Plan parse error: model returned empty response.\n\n\
             Model response was empty or whitespace-only. Try:\n\
             - Using a different model\n\
             - Simplifying the task\n\
             - Checking API connectivity"
        ));
    }

    // If the response doesn't start with '{' or '[', it's likely not JSON
    // (model returned an error message, plain text, or refused)
    let first_non_whitespace = json_text.chars().find(|c| !c.is_whitespace());
    if !matches!(first_non_whitespace, Some('{') | Some('[')) {
        return Err(anyhow::anyhow!(
            "Plan parse error: model did not return JSON.\n\n\
             Model response:\n{}\n\n\
             Expected JSON starting with {{ or [. The model may have refused, \
             returned an error, or not understood the format.",
            json_text
        ));
    }

    let raw: PlanResponse = serde_json::from_str(&json_text)
        .map_err(|e| anyhow::anyhow!("Plan parse error: {e}\n\nModel response:\n{json_text}"))?;

    let steps: Vec<PlanStep> = raw
        .steps
        .into_iter()
        .map(|s| {
            // Resolve any symbol names in files[] to real paths via the graph
            let mut resolved_files = graph.resolve_files(&s.files);

            // Enforce file count limits:
            // - Empty file lists are useless (step can't see anything)
            // - >10 files bloat context and degrade model attention
            if resolved_files.len() > 10 {
                // Truncate to 10 and log — the model should have split the step
                resolved_files.truncate(10);
            }

            PlanStep {
                description: s.description,
                instruction: s.instruction,
                files: resolved_files,
                verify: parse_verification(&s.verify),
                status: StepStatus::Pending,
                tool_budget: s.tool_budget.min(25), // cap tool budget to prevent runaway steps
                user_annotation: None,
                completed_summary: None,
            }
        })
        .collect();

    if steps.is_empty() {
        return Err(anyhow::anyhow!("Model returned an empty plan"));
    }

    let mut steps = steps;
    enrich_step_instructions(&mut steps, graph);

    Ok(Plan::new(task.to_string(), steps, project.to_string()))
}

/// Write the plan as human-readable markdown to `.parecode/plan.md`.
/// Overwrites any previous plan file — only the latest plan is kept.
/// Silently ignores errors (disk write must never crash planning).
pub fn write_plan_to_disk(plan: &Plan) {
    let _ = try_write_plan_to_disk(plan);
}

fn try_write_plan_to_disk(plan: &Plan) -> anyhow::Result<()> {
    std::fs::create_dir_all(".parecode")?;
    let mut md = format!("# Plan: {}\n\n", plan.task);

    for (i, step) in plan.steps.iter().enumerate() {
        md.push_str(&format!("## Step {}: {}\n\n", i + 1, step.description));
        md.push_str(&format!("{}\n\n", step.instruction));
        if !step.files.is_empty() {
            md.push_str(&format!("**Files:** {}\n\n", step.files.join(", ")));
        }
        let verify_str = match &step.verify {
            Verification::None => None,
            Verification::FileChanged(path) => Some(format!("file changed: `{path}`")),
            Verification::PatternAbsent { file, pattern } => Some(format!("`{file}` does not contain `{pattern}`")),
            Verification::CommandSuccess(cmd) => Some(format!("`{cmd}` exits 0")),
            Verification::BuildSuccess => Some("build succeeds".to_string()),
        };
        if let Some(v) = verify_str {
            md.push_str(&format!("**Verify:** {v}\n\n"));
        }
    }

    md.push_str("---\n*Generated by PareCode — edit annotations above, then confirm in TUI to execute.*\n");
    std::fs::write(".parecode/plan.md", md)?;
    Ok(())
}

// ── Step execution ────────────────────────────────────────────────────────────

/// Execute a single plan step.
///
/// Runs a fresh agent call with only `step.files` loaded as context.
/// `prior_summaries` is a list of (description, summary) from completed steps,
/// injected as a compact preamble so the model knows what changed before this step.
/// Returns `Ok(())` on success, `Err(msg)` on failure.
/// TESTING IS ESSENtiAL TO BE IMPROVED!! 
pub async fn execute_step(
    step: &PlanStep,
    client: &Client,
    config: &crate::agent::AgentConfig,
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    // Load the step's files as (path, formatted_content) pairs.
    // Use format_for_context so large files get the preamble+symbol-index+tail
    // treatment with line numbers — not a raw dump. This gives the model the
    // structural landmarks (exact line numbers for every function) it needs to
    // anchor edit_file calls correctly without reading the whole file again.
    let mut attached: Vec<String> = Vec::new();
    for path in &step.files {
        attached.push(path.clone());
    }
    

    let instruction = step.effective_instruction();

    // Run the agent with fresh context
    crate::agent::run_tui(
        &instruction,
        client,
        config,
        attached,
        vec![], // executor steps don't need session history
        ui_tx,
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::cache::FileCache::default())),
    )
    .await
}

/// Build a rich summary of what a step actually did, by inspecting the files
/// it listed. Includes: which files were modified, line count changes, new
/// symbols added, and structural notes (test modules, etc.). This summary is
/// injected into subsequent steps as the ONLY information they have about prior
/// work, so it must be detailed enough for the model to interact with those
/// files correctly without re-reading them.
pub fn summarise_completed_step(step: &PlanStep) -> String {
    if step.files.is_empty() {
        return format!("completed: {}", step.description);
    }

    let now = std::time::SystemTime::now();
    let mut parts: Vec<String> = Vec::new();

    for path in &step.files {
        let Ok(meta) = std::fs::metadata(path) else { continue };
        let Ok(modified) = meta.modified() else { continue };
        let age = now.duration_since(modified).unwrap_or(std::time::Duration::from_secs(999));

        // Only report files touched within the last 5 minutes
        if age.as_secs() > 300 {
            parts.push(format!("{path}: unchanged"));
            continue;
        }

        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Extract ALL public symbols — the next step may need to import/call them
        let symbols: Vec<String> = lines
            .iter()
            .filter_map(|line| {
                let t = line.trim();
                // Rust
                if t.starts_with("pub fn ")        { return Some(format!("fn {}", t.split('(').next()?.trim_start_matches("pub fn "))); }
                if t.starts_with("fn ")            { return Some(format!("fn {}", t.split('(').next()?.trim_start_matches("fn "))); }
                if t.starts_with("pub struct ")    { return Some(format!("struct {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("pub enum ")      { return Some(format!("enum {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("pub trait ")     { return Some(format!("trait {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("pub type ")      { return Some(format!("type {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("impl ")          { return Some(format!("impl {}", t.split('{').next()?.trim_start_matches("impl ").trim())); }
                // TS/JS
                if t.starts_with("export function ") { return Some(format!("fn {}", t.split('(').next()?.trim_start_matches("export function "))); }
                if t.starts_with("export class ")  { return Some(format!("class {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("export interface ") { return Some(format!("interface {}", t.split_whitespace().nth(2)?)); }
                if t.starts_with("function ")      { return Some(format!("fn {}", t.split('(').next()?.trim_start_matches("function "))); }
                if t.starts_with("class ")         { return Some(format!("class {}", t.split_whitespace().nth(1)?)); }
                // Python
                if t.starts_with("def ")           { return Some(format!("def {}", t.split('(').next()?.trim_start_matches("def "))); }
                if t.starts_with("class ")         { return Some(format!("class {}", t.split('(').next().or(t.split(':').next())?.trim_start_matches("class "))); }
                // Go
                if t.starts_with("func ")          { return Some(format!("func {}", t.split('(').next()?.trim_start_matches("func "))); }
                None
            })
            .collect();

        // Detect structural blocks
        let mut structural_notes: Vec<String> = Vec::new();

        // Test module detection (Rust #[cfg(test)])
        let has_test_mod = lines.iter().any(|l| l.trim() == "#[cfg(test)]");
        if has_test_mod {
            let test_fns: Vec<&str> = lines.iter()
                .filter_map(|l| {
                    let t = l.trim();
                    if t.starts_with("fn test_") || t.starts_with("async fn test_") {
                        t.split('(').next()
                            .map(|s| s.trim_start_matches("async fn ").trim_start_matches("fn "))
                    } else {
                        None
                    }
                })
                .collect();
            let fns_str = if test_fns.is_empty() {
                "(empty)".to_string()
            } else {
                test_fns.join(", ")
            };
            structural_notes.push(format!(
                "has #[cfg(test)] mod tests [{fns_str}] — \
                 to add more tests use edit_file with old_str inside the module \
                 (NOT append=true). Use exact line content and hashes from pre-loaded file."
            ));
        }

        // JS/TS describe blocks
        let has_describe = lines.iter().any(|l| {
            let t = l.trim();
            t.starts_with("describe(") || t.starts_with("describe.only(")
        });
        if has_describe {
            structural_notes.push("has describe() test block — add tests inside it".to_string());
        }

        // Import/use statements — next step may need to know what's imported
        let imports: Vec<&str> = lines.iter()
            .filter_map(|l| {
                let t = l.trim();
                if t.starts_with("use ") || t.starts_with("import ") || t.starts_with("from ") {
                    Some(t)
                } else {
                    None
                }
            })
            .take(10)
            .collect();

        // Build file summary
        let mut desc = format!("{path} ({total_lines} lines)");

        if !symbols.is_empty() {
            // Show up to 15 symbols — enough for next step to know exports
            let shown: Vec<&str> = symbols.iter().map(|s| s.as_str()).take(15).collect();
            let more = if symbols.len() > 15 { format!(", +{} more", symbols.len() - 15) } else { String::new() };
            desc.push_str(&format!("\n    symbols: [{}{}]", shown.join(", "), more));
        }

        if !imports.is_empty() {
            desc.push_str(&format!("\n    imports: [{}]", imports.join("; ")));
        }

        if !structural_notes.is_empty() {
            for note in &structural_notes {
                desc.push_str(&format!("\n    note: {note}"));
            }
        }

        parts.push(desc);
    }

    if parts.is_empty() {
        format!("completed: {}", step.description)
    } else {
        parts.join("\n  ")
    }
}

/// Run verification for a completed step.
/// Returns `Ok(())` if verification passes, `Err(msg)` if it fails.
pub fn verify_step(step: &PlanStep) -> Result<()> {
    match &step.verify {
        Verification::None => Ok(()),

        Verification::FileChanged(path) => {
            // Check the file exists and was modified recently (within last 60s)
            let meta = std::fs::metadata(path)
                .map_err(|e| anyhow::anyhow!("verify: cannot stat {path}: {e}"))?;
            let modified = meta
                .modified()
                .map_err(|_| anyhow::anyhow!("verify: cannot get mtime for {path}"))?;
            let age = modified
                .elapsed()
                .unwrap_or(std::time::Duration::from_secs(999));
            if age.as_secs() > 60 {
                Err(anyhow::anyhow!(
                    "verify: {path} was not modified in the last 60s"
                ))
            } else {
                Ok(())
            }
        }

        Verification::PatternAbsent { file, pattern } => {
            let content = std::fs::read_to_string(file)
                .map_err(|e| anyhow::anyhow!("verify: cannot read {file}: {e}"))?;
            if content.contains(pattern.as_str()) {
                let count = content.matches(pattern.as_str()).count();
                Err(anyhow::anyhow!(
                    "verify: pattern '{pattern}' still found in {file} ({count} occurrences)"
                ))
            } else {
                Ok(())
            }
        }

        Verification::CommandSuccess(cmd) => {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .map_err(|e| anyhow::anyhow!("verify: failed to run '{cmd}': {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let lines: Vec<&str> = combined.lines().take(30).collect();
                Err(anyhow::anyhow!(
                    "verify: '{cmd}' failed (exit {}):\n{}",
                    output.status.code().unwrap_or(-1),
                    lines.join("\n")
                ))
            }
        }

        Verification::BuildSuccess => {
            // BuildSuccess without a specific command: pass.
            // Use Verification::CommandSuccess("your build cmd") for language-specific checks.
            Ok(())
        }
    }
}

// ── JSON extraction helpers ───────────────────────────────────────────────────

/// Extract valid JSON from a potentially malformed response.
/// Handles cases where models return:
/// - Duplicate opening braces: `{ { "key": ...` or `{ [ { "key": ...`
/// - Extra whitespace between structural chars
/// - Markdown fences that weren't fully stripped
///
/// Returns the JSON substring that starts at the first `{` or `[` and ends
/// at the matching closing brace.
fn extract_json(input: &str) -> Option<String> {
    let input = input.trim();
    
    // More aggressive markdown stripping: handle ```json, ```json {, ```json { {
    let mut input = input;
    loop {
        let was = input;
        input = input.trim_start_matches("```json").trim_start_matches("```").trim_start();
        if input.len() == was.len() { break; }
    }
    // Also strip trailing ```
    input = input.trim_end_matches("```").trim_end();
    
    // Find the first opening brace or bracket
    let start = input.find(|c| c == '{' || c == '[')?;
    let input = input.get(start..)?;
    
    // Handle duplicate opening braces/brackets: `{ {`, `{ [`, `[ {`, `[ [`
    // Some models emit `{ { "key": ...` which is invalid JSON
    let input = collapse_duplicate_braces(input)?;
    
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut end_idx = 0;
    
    for (i, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end_idx = i;
                    break;
                }
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end_idx = i;
                    break;
                }
            }
            _ => {}
        }
    }
    
    if depth == 0 {
        Some(input[..=end_idx].to_string())
    } else {
        None
    }
}

/// Collapse duplicate opening braces/brackets at the start of JSON.
/// Handles: `{ {`, `{ [`, `[ {`, `[ [`, with optional whitespace between.
/// Also handles multiple duplicates: `{ { {` → `{`
fn collapse_duplicate_braces(input: &str) -> Option<String> {
    let mut chars = input.chars().peekable();
    
    // Must start with { or [
    let first = chars.next()?;
    if first != '{' && first != '[' {
        return None;
    }
    
    // Skip whitespace and check for duplicates
    let mut has_duplicate = false;
    loop {
        match chars.peek() {
            Some(&c) if c.is_whitespace() => {
                chars.next();
            }
            Some(&'{') | Some(&'[') => {
                has_duplicate = true;
                chars.next(); // skip the duplicate
            }
            _ => break,
        }
    }
    
    if has_duplicate {
        // Rebuild string without duplicates
        let remaining: String = chars.collect();
        Some(format!("{first}{remaining}"))
    } else {
        Some(input.to_string())
    }
}

// ── JSON sanitizer ────────────────────────────────────────────────────────────

/// Replace unescaped control characters inside JSON string values with their
/// proper JSON escape sequences. Small models often emit literal newlines/tabs
/// inside string values, producing invalid JSON that serde_json rejects.
///
/// Walks the input char-by-char tracking in/out of string literals, handling
/// backslash escapes correctly so we don't double-escape already-escaped chars.
fn sanitize_json_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            out.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if in_string {
            match ch {
                '\n' => { out.push_str("\\n"); continue; }
                '\r' => { out.push_str("\\r"); continue; }
                '\t' => { out.push_str("\\t"); continue; }
                c if (c as u32) < 0x20 => {
                    // Other control chars: emit as \uXXXX
                    out.push_str(&format!("\\u{:04x}", c as u32));
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JSON extraction ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_json_with_duplicate_braces() {
        // Model returns `{ { "description": "test" }` - duplicate opening brace
        let input = r#"{ { "description": "test step", "instruction": "do something", "files": ["foo.rs"], "verify": "none", "tool_budget": 10 }"#;
        let result = extract_json(input);
        assert!(result.is_some(), "Should extract JSON even with duplicate brace");
        
        let json = result.unwrap();
        // Should be valid JSON that can be parsed
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("Should be valid JSON after fixing duplicate braces");
        assert_eq!(parsed["description"], "test step");
    }

    #[test]
    fn test_extract_json_with_markdown_fence() {
        let input = r#"```json
{ "description": "test" }
```"#;
        let result = extract_json(input);
        assert!(result.is_some());
        
        let json = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("Should parse after stripping fence");
        assert_eq!(parsed["description"], "test");
    }

    #[test]
    fn test_extract_json_with_whitespace_after_fence() {
        // Model returns ```json { { "description": ...
        let input = "```json\n{ { \"description\": \"test\" }";
        let result = extract_json(input);
        assert!(result.is_some());
        
        let json = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("Should parse");
        assert_eq!(parsed["description"], "test");
    }

    #[test]
    fn test_extract_json_complex() {
        // Full plan structure with duplicate brace issue
        let input = r#"some text before
```json
{ { "steps": [ { "description": "First step", "instruction": "do x", "files": ["a.rs"], "verify": "none", "tool_budget": 10 } ] }
```
some text after"#;
        
        let result = extract_json(input);
        assert!(result.is_some());
        
        let json = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("Should be valid JSON");
        assert_eq!(parsed["steps"][0]["description"], "First step");
    }
}
