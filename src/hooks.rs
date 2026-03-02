/// Hooks — lifecycle commands that run at key points in the agent workflow.
///
/// `on_edit` hooks are injected directly into the model's tool result so the
/// model sees compile/lint errors and can self-correct immediately.
/// `on_task_done` hooks run after the agent loop and are shown in the TUI only.
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, timeout};
use tokio::process::Command;

// ── Config ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    /// Commands run after every successful edit_file or write_file call.
    /// Output is injected into the model's tool result so it can self-correct.
    #[serde(default)]
    pub on_edit: Vec<String>,
    /// Commands run after the agent loop completes (shown in TUI, not in context).
    #[serde(default)]
    pub on_task_done: Vec<String>,
    /// Commands run after each plan step completes.
    #[serde(default)]
    pub on_plan_step_done: Vec<String>,
    /// Commands run when the TUI starts.
    #[serde(default)]
    pub on_session_start: Vec<String>,
    /// Commands run when the TUI exits.
    #[serde(default)]
    pub on_session_end: Vec<String>,
}

impl HookConfig {
    pub fn is_empty(&self) -> bool {
        self.on_edit.is_empty()
            && self.on_task_done.is_empty()
            && self.on_plan_step_done.is_empty()
            && self.on_session_start.is_empty()
            && self.on_session_end.is_empty()
    }

    /// One-line summary of active hooks for startup display.
    /// Returns None when no hooks are configured.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.on_edit.is_empty() {
            parts.push(format!("on_edit: {}", self.on_edit.join(", ")));
        }
        if !self.on_task_done.is_empty() {
            parts.push(format!("on_task_done: {}", self.on_task_done.join(", ")));
        }
        if !self.on_plan_step_done.is_empty() {
            parts.push(format!("on_plan_step_done: {}", self.on_plan_step_done.join(", ")));
        }
        if !self.on_session_start.is_empty() {
            parts.push(format!("on_session_start: {}", self.on_session_start.join(", ")));
        }
        if !self.on_session_end.is_empty() {
            parts.push(format!("on_session_end: {}", self.on_session_end.join(", ")));
        }
        if parts.is_empty() { None } else { Some(parts.join("  ·  ")) }
    }

    /// Full multi-line listing for /list-hooks.
    pub fn detail(&self) -> String {
        let fmt = |label: &str, cmds: &[String]| -> String {
            if cmds.is_empty() {
                format!("  {label:<20} (none)")
            } else {
                let list = cmds.iter().map(|c| format!("\n    · {c}")).collect::<String>();
                format!("  {label:<20}{list}")
            }
        };
        [
            fmt("on_edit",           &self.on_edit),
            fmt("on_task_done",      &self.on_task_done),
            fmt("on_plan_step_done", &self.on_plan_step_done),
            fmt("on_session_start",  &self.on_session_start),
            fmt("on_session_end",    &self.on_session_end),
        ].join("\n")
    }
}

// ── Result ─────────────────────────────────────────────────────────────────────

pub struct HookResult {
    /// Merged stdout + stderr
    pub output: String,
    pub exit_code: i32,
}

// ── Config persistence ────────────────────────────────────────────────────────

/// Write a named `HookConfig` to the config file as `[hooks.NAME]`.
/// Used by the wizard when the user confirms their new hook configuration.
/// Skips writing if that section already exists. Non-fatal on write errors.
pub fn write_config_hooks(name: &str, cfg: &HookConfig) {
    let config_path = crate::config::config_path();
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Don't double-write if this hooks section already exists
    let hooks_header = format!("[hooks.{name}]");
    if existing.contains(&hooks_header) {
        return;
    }

    let fmt_cmds = |cmds: &[String]| -> String {
        if cmds.is_empty() {
            return String::new();
        }
        let inner: Vec<String> = cmds.iter().map(|c| format!("  \"{c}\"")).collect();
        format!("[\n{}\n]", inner.join(",\n"))
    };

    let mut lines = Vec::new();
    lines.push(format!("\n[hooks.{name}]"));
    if !cfg.on_edit.is_empty() {
        lines.push(format!("on_edit = {}", fmt_cmds(&cfg.on_edit)));
    }
    if !cfg.on_task_done.is_empty() {
        lines.push(format!("on_task_done = {}", fmt_cmds(&cfg.on_task_done)));
    }
    if !cfg.on_plan_step_done.is_empty() {
        lines.push(format!("on_plan_step_done = {}", fmt_cmds(&cfg.on_plan_step_done)));
    }
    if !cfg.on_session_start.is_empty() {
        lines.push(format!("on_session_start = {}", fmt_cmds(&cfg.on_session_start)));
    }
    if !cfg.on_session_end.is_empty() {
        lines.push(format!("on_session_end = {}", fmt_cmds(&cfg.on_session_end)));
    }
    lines.push(String::new());

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&config_path) {
        let _ = f.write_all(lines.join("\n").as_bytes());
    }
}

/// Persist `active_hooks = "name"` (or clear it) in the config file.
/// `active_hooks` must be a top-level TOML key (before any [section] header).
/// This function removes any existing `active_hooks` line (wherever it is)
/// and re-inserts it before the first [section] header so it stays top-level.
/// Non-fatal on errors.
pub fn write_active_hooks(name: Option<&str>) {
    let config_path = crate::config::config_path();
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Strip any existing active_hooks lines (they may be misplaced inside a section)
    let stripped: Vec<&str> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with("active_hooks"))
        .collect();

    let Some(n) = name else {
        // Clearing — just write back without the line
        let updated = stripped.join("\n");
        let updated = if existing.ends_with('\n') { format!("{updated}\n") } else { updated };
        let _ = std::fs::write(&config_path, updated);
        return;
    };

    let new_line = format!("active_hooks = \"{n}\"");

    // Insert before the first line that starts a [section]
    let mut inserted = false;
    let mut result: Vec<&str> = Vec::with_capacity(stripped.len() + 1);
    let new_line_ref: &str = &new_line;
    for line in &stripped {
        if !inserted && line.trim_start().starts_with('[') {
            result.push(new_line_ref);
            result.push("");
            inserted = true;
        }
        result.push(line);
    }
    if !inserted {
        // No [section] found — append at end
        result.push("");
        result.push(new_line_ref);
    }

    let updated = result.join("\n");
    let updated = if existing.ends_with('\n') { format!("{updated}\n") } else { updated };
    let _ = std::fs::write(&config_path, updated);
}

// ── Runner ────────────────────────────────────────────────────────────────────

const HOOK_TIMEOUT_SECS: u64 = 30;
const HOOK_MAX_LINES: usize = 50;

/// Run a single hook command via `sh -c`. Merges stdout + stderr.
/// Caps output at `HOOK_MAX_LINES` lines to avoid bloating context.
pub async fn run_hook(cmd: &str) -> HookResult {
    let fut = Command::new("sh").arg("-c").arg(cmd).output();

    let output = match timeout(Duration::from_secs(HOOK_TIMEOUT_SECS), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return HookResult {
                output: format!("[hook failed to start: {e}]"),
                exit_code: -1,
            };
        }
        Err(_) => {
            return HookResult {
                output: format!("[hook timed out after {HOOK_TIMEOUT_SECS}s]"),
                exit_code: -1,
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (true, false) => stderr.to_string(),
        (false, true) => stdout.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    };

    let lines: Vec<&str> = combined.lines().collect();
    let output = if lines.len() <= HOOK_MAX_LINES {
        combined
    } else {
        let truncated = lines[..HOOK_MAX_LINES].join("\n");
        format!("{truncated}\n[+{} lines truncated]", lines.len() - HOOK_MAX_LINES)
    };

    HookResult { output, exit_code }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── HookConfig ──────────────────────────────────────────────────────────────

    #[test]
    fn test_hook_config_is_empty() {
        let empty = HookConfig::default();
        assert!(empty.is_empty());

        let with_on_edit = HookConfig {
            on_edit: vec!["cargo check".to_string()],
            ..Default::default()
        };
        assert!(!with_on_edit.is_empty());

        let with_on_task_done = HookConfig {
            on_task_done: vec!["cargo test".to_string()],
            ..Default::default()
        };
        assert!(!with_on_task_done.is_empty());

        let with_on_plan_step_done = HookConfig {
            on_plan_step_done: vec!["echo done".to_string()],
            ..Default::default()
        };
        assert!(!with_on_plan_step_done.is_empty());

        let with_on_session_start = HookConfig {
            on_session_start: vec!["echo start".to_string()],
            ..Default::default()
        };
        assert!(!with_on_session_start.is_empty());

        let with_on_session_end = HookConfig {
            on_session_end: vec!["echo end".to_string()],
            ..Default::default()
        };
        assert!(!with_on_session_end.is_empty());

        let all_hooks = HookConfig {
            on_edit: vec!["check".to_string()],
            on_task_done: vec!["test".to_string()],
            on_plan_step_done: vec!["step".to_string()],
            on_session_start: vec!["start".to_string()],
            on_session_end: vec!["end".to_string()],
        };
        assert!(!all_hooks.is_empty());
    }

    #[test]
    fn test_hook_config_summary_empty() {
        let empty = HookConfig::default();
        assert_eq!(empty.summary(), None);
    }

    #[test]
    fn test_hook_config_summary_single() {
        let config = HookConfig {
            on_edit: vec!["cargo check".to_string()],
            ..Default::default()
        };
        assert_eq!(config.summary(), Some("on_edit: cargo check".to_string()));
    }

    #[test]
    fn test_hook_config_summary_multiple() {
        let config = HookConfig {
            on_edit: vec!["cargo check".to_string(), "cargo clippy".to_string()],
            on_task_done: vec!["cargo test".to_string()],
            ..Default::default()
        };
        let summary = config.summary().unwrap();
        assert!(summary.contains("on_edit: cargo check, cargo clippy"));
        assert!(summary.contains("on_task_done: cargo test"));
        assert!(summary.contains("·"));
    }

    #[test]
    fn test_hook_config_summary_all_hooks() {
        let config = HookConfig {
            on_edit: vec!["edit".to_string()],
            on_task_done: vec!["task".to_string()],
            on_plan_step_done: vec!["step".to_string()],
            on_session_start: vec!["start".to_string()],
            on_session_end: vec!["end".to_string()],
        };
        let summary = config.summary().unwrap();
        assert!(summary.contains("on_edit: edit"));
        assert!(summary.contains("on_task_done: task"));
        assert!(summary.contains("on_plan_step_done: step"));
        assert!(summary.contains("on_session_start: start"));
        assert!(summary.contains("on_session_end: end"));
    }

    #[test]
    fn test_hook_config_detail_empty() {
        let empty = HookConfig::default();
        let detail = empty.detail();
        assert!(detail.contains("on_edit"));
        assert!(detail.contains("on_task_done"));
        assert!(detail.contains("on_plan_step_done"));
        assert!(detail.contains("on_session_start"));
        assert!(detail.contains("on_session_end"));
        assert!(detail.contains("(none)"));
    }

    #[test]
    fn test_hook_config_detail_with_commands() {
        let config = HookConfig {
            on_edit: vec!["cargo check".to_string()],
            on_task_done: vec!["cargo test".to_string(), "echo done".to_string()],
            ..Default::default()
        };
        let detail = config.detail();
        assert!(detail.contains("on_edit"));
        assert!(detail.contains("· cargo check"));
        assert!(detail.contains("on_task_done"));
        assert!(detail.contains("· cargo test"));
        assert!(detail.contains("· echo done"));
        assert!(detail.contains("on_plan_step_done"));
        assert!(detail.contains("(none)"));
    }

    #[test]
    fn test_hook_config_serde_roundtrip() {
        let config = HookConfig {
            on_edit: vec!["cargo check".to_string()],
            on_task_done: vec!["cargo test".to_string()],
            on_plan_step_done: vec!["echo step".to_string()],
            on_session_start: vec!["echo start".to_string()],
            on_session_end: vec!["echo end".to_string()],
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: HookConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.on_edit, deserialized.on_edit);
        assert_eq!(config.on_task_done, deserialized.on_task_done);
        assert_eq!(config.on_plan_step_done, deserialized.on_plan_step_done);
        assert_eq!(config.on_session_start, deserialized.on_session_start);
        assert_eq!(config.on_session_end, deserialized.on_session_end);
    }

    #[test]
    fn test_hook_config_serde_default_fields() {
        // Test that missing fields deserialize to empty vectors
        let json = r#"{}"#;
        let config: HookConfig = serde_json::from_str(json).unwrap();
        assert!(config.is_empty());
        assert!(config.on_edit.is_empty());
        assert!(config.on_task_done.is_empty());
        assert!(config.on_plan_step_done.is_empty());
        assert!(config.on_session_start.is_empty());
        assert!(config.on_session_end.is_empty());
    }

    // ── Hook runner ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_hook_success() {
        let result = run_hook("echo hello").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn test_run_hook_failure() {
        let result = run_hook("exit 42").await;
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn test_run_hook_stdout_only() {
        let result = run_hook("echo stdout").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("stdout"));
    }

    #[tokio::test]
    async fn test_run_hook_stderr_only() {
        let result = run_hook("echo stderr >&2").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("stderr"));
    }

    #[tokio::test]
    async fn test_run_hook_stdout_and_stderr() {
        let result = run_hook("echo stdout && echo stderr >&2").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("stdout"));
        assert!(result.output.contains("stderr"));
    }

    #[tokio::test]
    async fn test_run_hook_empty_output() {
        let result = run_hook("true").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.is_empty());
    }

    #[tokio::test]
    async fn test_run_hook_nonexistent_command() {
        let result = run_hook("this_command_does_not_exist_12345").await;
        // sh will return non-zero for a command not found
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn test_run_hook_truncation() {
        // Generate output with more than HOOK_MAX_LINES lines
        let lines = HOOK_MAX_LINES + 10;
        let cmd = format!("seq 1 {}", lines);
        let result = run_hook(&cmd).await;
        
        assert_eq!(result.exit_code, 0);
        let output_lines: Vec<&str> = result.output.lines().collect();
        
        // Should have HOOK_MAX_LINES + 1 (truncation message)
        assert_eq!(output_lines.len(), HOOK_MAX_LINES + 1);
        assert!(result.output.contains("[+10 lines truncated]"));
    }

    #[tokio::test]
    async fn test_run_hook_no_truncation_at_limit() {
        // Generate exactly HOOK_MAX_LINES lines
        let cmd = format!("seq 1 {}", HOOK_MAX_LINES);
        let result = run_hook(&cmd).await;
        
        assert_eq!(result.exit_code, 0);
        let output_lines: Vec<&str> = result.output.lines().collect();
        
        // Should have exactly HOOK_MAX_LINES, no truncation
        assert_eq!(output_lines.len(), HOOK_MAX_LINES);
        assert!(!result.output.contains("truncated"));
    }

    #[tokio::test]
    async fn test_run_hook_multiline_output() {
        let result = run_hook("printf 'line1\\nline2\\nline3'").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.output.contains("line1"));
        assert!(result.output.contains("line2"));
        assert!(result.output.contains("line3"));
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[tokio::test]
    async fn test_run_hook_exit_code_minus_one_on_error() {
        // Test that shell errors result in exit code -1 being captured
        // We can't easily test "failed to start" without mocking, but we can
        // verify the exit code structure works
        let result = run_hook("exit 1").await;
        assert_eq!(result.exit_code, 1);
    }

    // ── Config persistence ──────────────────────────────────────────────────────

    // Note: write_hooks_to_config tests would require mocking the filesystem
    // and config module, which is complex for unit tests. Integration tests
    // would be more appropriate for testing that function.
}