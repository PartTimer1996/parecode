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

use crate::client::{Client, Message, MessageContent};
use crate::index::SymbolIndex;
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

/// The system prompt used specifically for plan generation.
/// Tighter than the regular agent prompt — focused on producing structured output.
const PLAN_SYSTEM_PROMPT: &str = r#"You are PareCode, a coding assistant. Your task is to produce a structured execution plan as JSON.

The plan breaks a coding task into discrete, independently executable steps.

CRITICAL rules:
- Each step runs in TOTAL ISOLATION with ONLY the files listed in its "files" array visible — it cannot see any other files or conversation history
- List EVERY file the step needs to read OR modify, including files that define types, interfaces, or modules it depends on
- Maximum 10 files per step (steps with more than 10 files will be rejected — split into smaller steps instead)
- Minimum 1 file per step (steps with no files are useless)
- The "instruction" field is the model's ONLY context — be precise about what to change, where, and why. Include specifics like function signatures, struct field names, or API shapes the step needs to produce. Do NOT say "look at file X" — describe what the step will find there
- After each step completes, subsequent steps receive a summary of what changed (modified files, symbols added, structural notes) — but NOT the actual file contents from that step, so instructions should be self-contained
- Prefer 4-8 steps; do not create micro-steps that split naturally-coupled changes
- Each step should be independently verifiable — prefer "command:cargo test" or similar where applicable
- DO NOT create read_file or search tool steps — they will not work in isolated execution. Instead, during planning, discover everything each step needs (file paths, function signatures, exact line numbers, struct fields, imports) and bake that information directly into the "instruction" field. Reference specific locations like "add `process_request` after the `handle_connection` function at line 45 in src/server.rs". The executor will pre-load the files listed in "files" — do not re-read them at runtime.

Respond with ONLY valid JSON — no markdown fences, no explanation. Format:

{
  "steps": [
    {
      "description": "human-readable one-liner shown to user",
      "instruction": "precise model-facing instruction — include enough detail that the step can execute without seeing other steps",
      "files": ["src/foo.rs", "src/types.rs", "src/bar.rs"],
      "verify": "none",
      "tool_budget": 15
    }
  ]
}

For "verify", use one of:
- "none" — no automated verification
- "command:some command" — run a specific command, expect exit 0
- "absent:file.ts:old_pattern" — check pattern no longer exists in file
- "changed:file.ts" — check file was modified"#;

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

/// Call the model to generate a plan for `task`.
/// Streams nothing — waits for the full response then parses JSON.
/// Returns the plan steps on success, or an error string for display.
pub async fn generate_plan(
    task: &str,
    client: &Client,
    project: &str,
    context_files: &[(String, String)], // (path, content) attached files as context
    index: &SymbolIndex,
    on_chunk: impl Fn(&str) + Send + 'static,
) -> Result<Plan> {
    // Build the user message: task + any attached file context
    let mut user_content = String::new();

    // Inject symbol index — gives the model an accurate file map to reference
    if let Some(index_section) = index.to_prompt_section(60) {
        user_content.push_str(&index_section);
        user_content.push('\n');
    }

    if !context_files.is_empty() {
        user_content.push_str("The following files are attached:\n\n");
        for (path, content) in context_files {
            let total = content.lines().count();
            // Show up to 300 lines — enough to see structure and imports
            let preview: String = content
                .lines()
                .take(300)
                .collect::<Vec<_>>()
                .join("\n");
            let note = if total > 300 {
                format!(" ({total} lines total, showing first 300)")
            } else {
                String::new()
            };
            user_content.push_str(&format!("[{path}{note}]\n{preview}\n\n"));
        }
        user_content.push_str("---\n\n");
    }

    user_content.push_str(&format!(
        "Generate a plan to accomplish this task:\n\n{task}"
    ));

    let messages = vec![Message {
        role: "user".to_string(),
        content: MessageContent::Text(user_content),
        tool_calls: vec![],
    }];

    // No tools during planning — pure text response
    let response = client
        .chat(PLAN_SYSTEM_PROMPT, &messages, &[], on_chunk)
        .await?;

    // Parse the JSON response
    let json_text_raw = response.text.trim();

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
            // Resolve any symbol names in files[] to real paths via the index
            let mut resolved_files = index.resolve_files(&s.files);

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
pub async fn execute_step(
    step: &PlanStep,
    client: &Client,
    config: &crate::agent::AgentConfig,
    prior_summaries: &[(String, String)], // (step description, what was done)
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> Result<()> {
    // Load the step's files as (path, formatted_content) pairs.
    // Use format_for_context so large files get the preamble+symbol-index+tail
    // treatment with line numbers — not a raw dump. This gives the model the
    // structural landmarks (exact line numbers for every function) it needs to
    // anchor edit_file calls correctly without reading the whole file again.
    let mut attached: Vec<(String, String)> = Vec::new();
    for path in &step.files {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let formatted = crate::tools::read::format_for_context(path, &raw);
                attached.push((path.clone(), formatted));
            }
            Err(e) => {
                // Non-fatal — model will get an error if it tries to read the file
                let _ = ui_tx.send(UiEvent::ToolResult {
                    summary: format!("⚠ could not pre-load {path}: {e}"),
                });
            }
        }
    }

    let instruction = step.effective_instruction();

    // Build prior-step context preamble if there are completed steps
    let prior_context = if prior_summaries.is_empty() {
        None
    } else {
        let lines: String = prior_summaries
            .iter()
            .enumerate()
            .map(|(i, (desc, summary))| format!("Step {}: {}\n  → {}", i + 1, desc, summary))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "# Completed steps so far\n{lines}\n\nThe above changes are already in place. Do not redo them.\n\n---\n\n"
        ))
    };

    // Run the agent with fresh context
    crate::agent::run_tui(
        &instruction,
        client,
        config,
        attached,
        prior_context,
        ui_tx,
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
