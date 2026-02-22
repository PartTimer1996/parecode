/// Plan/Execute architecture for Forge — Phase 4.
///
/// A Plan is a `Vec<PlanStep>` owned by the scaffold. Each step executes with a
/// fresh context containing only its specific files. The model never sees history
/// from previous steps — only its current bounded instruction.
///
/// Flow:
///   1. User types `/plan "task"`
///   2. Forge calls the model once to generate a structured plan (JSON)
///   3. TUI shows plan review overlay — user can annotate steps or approve
///   4. Each step runs as an isolated agent call (fresh context, step files only)
///   5. After each step, verification runs; failure surfaces for user decision
///
/// Plans are persisted to `.forge/plans/{timestamp}.json` so they can be resumed.
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
    /// Run `cargo build` or equivalent and check exit code 0
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

    pub fn pending_count(&self) -> usize {
        self.steps.iter().filter(|s| s.status == StepStatus::Pending).count()
    }

    pub fn completed_count(&self) -> usize {
        self.steps.iter().filter(|s| s.status == StepStatus::Pass).count()
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

/// Directory for saved plans: `.forge/plans/` relative to cwd.
pub fn plans_dir() -> PathBuf {
    PathBuf::from(".forge/plans")
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

pub fn load_plan(path: &std::path::Path) -> Result<Plan> {
    let json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

/// Find the most recent plan in `.forge/plans/`
pub fn find_latest_plan() -> Option<PathBuf> {
    let dir = plans_dir();
    if !dir.exists() {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| {
            e.path().extension().map(|x| x == "json").unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    entries.first().map(|e| e.path())
}

// ── Plan generation ───────────────────────────────────────────────────────────

/// The system prompt used specifically for plan generation.
/// Tighter than the regular agent prompt — focused on producing structured output.
const PLAN_SYSTEM_PROMPT: &str = r#"You are Forge, a coding assistant. Your task is to produce a structured execution plan as JSON.

The plan breaks a coding task into discrete, independently executable steps.

Rules for good plans:
- Each step should do exactly ONE thing (read, edit, verify — not all three)
- List only the files genuinely needed for that step in "files" (1-3 files per step is ideal)
- The "instruction" field is what the model will receive as its entire context — make it self-contained and precise
- Keep steps small: prefer 4-8 steps over 2 giant steps
- The last step should always verify the result (search, build check, or test run)

Respond with ONLY valid JSON — no markdown fences, no explanation. Format:

{
  "steps": [
    {
      "description": "human-readable one-liner shown to user",
      "instruction": "precise model-facing instruction with full context needed",
      "files": ["relative/path/to/file.ts"],
      "verify": "none",
      "tool_budget": 5
    }
  ]
}

For "verify", use one of:
- "none" — no automated verification
- "build" — run cargo build / npm build / equivalent
- "command:cargo test" — run a specific command, expect exit 0
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
fn default_tool_budget() -> usize { 8 }

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
) -> Result<Plan> {
    // Build the user message: task + any attached file context
    let mut user_content = String::new();

    // Inject symbol index — gives the model an accurate file map to reference
    if let Some(index_section) = index.to_prompt_section(60) {
        user_content.push_str(&index_section);
        user_content.push('\n');
    }

    if !context_files.is_empty() {
        user_content.push_str("The following files are available in this project:\n\n");
        for (path, content) in context_files {
            // Only show first 80 lines per file — enough for structure without bloat
            let preview: String = content
                .lines()
                .take(80)
                .collect::<Vec<_>>()
                .join("\n");
            let total = content.lines().count();
            let note = if total > 80 {
                format!(" ({total} lines total, showing first 80)")
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
    }];

    // No tools during planning — pure text response
    let response = client
        .chat(PLAN_SYSTEM_PROMPT, &messages, &[], |_chunk| {})
        .await?;

    // Parse the JSON response
    let json_text = response.text.trim();

    // Strip markdown fences if the model wrapped it despite instructions
    let json_text = json_text
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let raw: PlanResponse = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("Plan parse error: {e}\n\nModel response:\n{json_text}"))?;

    let steps: Vec<PlanStep> = raw
        .steps
        .into_iter()
        .map(|s| {
            // Resolve any symbol names in files[] to real paths via the index
            let resolved_files = index.resolve_files(&s.files);
            PlanStep {
                description: s.description,
                instruction: s.instruction,
                files: resolved_files,
                verify: parse_verification(&s.verify),
                status: StepStatus::Pending,
                tool_budget: s.tool_budget,
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

/// Write the plan as human-readable markdown to `.forge/plan.md`.
/// Overwrites any previous plan file — only the latest plan is kept.
/// Silently ignores errors (disk write must never crash planning).
pub fn write_plan_to_disk(plan: &Plan) {
    let _ = try_write_plan_to_disk(plan);
}

fn try_write_plan_to_disk(plan: &Plan) -> anyhow::Result<()> {
    std::fs::create_dir_all(".forge")?;
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

    md.push_str("---\n*Generated by Forge — edit annotations above, then confirm in TUI to execute.*\n");
    std::fs::write(".forge/plan.md", md)?;
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
    // Load the step's files as (path, content) pairs
    let mut attached: Vec<(String, String)> = Vec::new();
    for path in &step.files {
        match std::fs::read_to_string(path) {
            Ok(content) => attached.push((path.clone(), content)),
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

/// Build a compact summary of what a step actually did, by inspecting the files
/// it listed. Reports which files were recently modified and their top symbols.
/// Falls back to the step description if files can't be read.
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
            continue;
        }

        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let symbols: Vec<&str> = content
            .lines()
            .filter_map(|line| {
                let t = line.trim();
                // Rust / TS / Python / Go top-level symbols
                if t.starts_with("pub fn ")    { return Some(t.split('(').next()?.trim_start_matches("pub fn ")); }
                if t.starts_with("fn ")        { return Some(t.split('(').next()?.trim_start_matches("fn ")); }
                if t.starts_with("pub struct ")    { return Some(t.split_whitespace().nth(2)?); }
                if t.starts_with("pub enum ")      { return Some(t.split_whitespace().nth(2)?); }
                if t.starts_with("pub trait ")     { return Some(t.split_whitespace().nth(2)?); }
                if t.starts_with("impl ")          { return Some(t.split_whitespace().nth(1)?); }
                if t.starts_with("export function ") { return Some(t.split('(').next()?.trim_start_matches("export function ")); }
                if t.starts_with("function ")    { return Some(t.split('(').next()?.trim_start_matches("function ")); }
                if t.starts_with("class ")       { return Some(t.split_whitespace().nth(1)?); }
                if t.starts_with("def ")         { return Some(t.split('(').next()?.trim_start_matches("def ")); }
                if t.starts_with("func ")        { return Some(t.split('(').next()?.trim_start_matches("func ")); }
                None
            })
            .take(4)
            .collect();

        if symbols.is_empty() {
            parts.push(format!("modified {path}"));
        } else {
            parts.push(format!("modified {path} [{}]", symbols.join(", ")));
        }
    }

    if parts.is_empty() {
        format!("completed: {}", step.description)
    } else {
        parts.join("; ")
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
                let stderr = String::from_utf8_lossy(&output.stderr);
                let first_err = stderr.lines().next().unwrap_or("(no output)");
                Err(anyhow::anyhow!(
                    "verify: '{cmd}' failed (exit {}): {first_err}",
                    output.status.code().unwrap_or(-1)
                ))
            }
        }

        Verification::BuildSuccess => {
            // Auto-detect build command
            let cmd = if std::path::Path::new("Cargo.toml").exists() {
                "cargo build 2>&1 | tail -5"
            } else if std::path::Path::new("package.json").exists() {
                "npm run build 2>&1 | tail -5"
            } else {
                return Ok(()); // Can't detect — skip
            };
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .map_err(|e| anyhow::anyhow!("verify: build failed to run: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                let out = String::from_utf8_lossy(&output.stdout);
                let first = out.lines().next().unwrap_or("build failed");
                Err(anyhow::anyhow!("verify: build failed: {first}"))
            }
        }
    }
}
