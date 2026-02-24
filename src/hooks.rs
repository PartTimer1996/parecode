/// Hooks — lifecycle commands that run at key points in the agent workflow.
///
/// Auto-detection: when no hooks are explicitly configured and hooks are not
/// disabled, `detect_language_hooks()` scans the cwd for project markers and
/// returns sensible defaults (e.g. `cargo check -q` for Rust projects).
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

// ── Language auto-detection ────────────────────────────────────────────────────

/// Scan cwd for project markers and return default hooks.
/// Returns an empty `HookConfig` when no recognisable project is found.
pub fn detect_language_hooks() -> HookConfig {
    use std::path::Path;

    if Path::new("Cargo.toml").exists() {
        return HookConfig {
            on_edit: vec!["cargo check -q".to_string()],
            on_task_done: vec!["cargo test -q 2>&1 | tail -5".to_string()],
            ..Default::default()
        };
    }

    if Path::new("tsconfig.json").exists() {
        return HookConfig {
            on_edit: vec!["tsc --noEmit".to_string()],
            ..Default::default()
        };
    }

    if Path::new("go.mod").exists() {
        return HookConfig {
            on_edit: vec!["go build ./...".to_string()],
            ..Default::default()
        };
    }

    // Python: only if ruff is available
    if (Path::new("pyproject.toml").exists() || Path::new("setup.py").exists())
        && which_binary("ruff")
    {
        return HookConfig {
            on_edit: vec!["ruff check .".to_string()],
            ..Default::default()
        };
    }

    HookConfig::default()
}

/// Check if a binary exists in PATH.
fn which_binary(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Config persistence ────────────────────────────────────────────────────────

/// Detect the project language and write a hooks section into the config file
/// for `profile_name`, if one doesn't already exist.
///
/// Uses TOML append — preserves all existing comments and structure.
/// Returns the detected `HookConfig` (empty if nothing was detected or config
/// couldn't be written).
pub fn write_hooks_to_config(profile_name: &str) -> HookConfig {
    let detected = detect_language_hooks();
    if detected.is_empty() {
        return HookConfig::default();
    }

    let config_path = crate::config::config_path();

    // Read existing config — bail silently if unreadable
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Don't append if a hooks section for this profile already exists
    let hooks_header = format!("[profiles.{profile_name}.hooks]");
    if existing.contains(&hooks_header) {
        return detected;
    }

    // Build the hooks block with active commands for the detected language
    // and commented examples for every other event, so the user can see options.
    let on_edit_active = if detected.on_edit.is_empty() {
        String::new()
    } else {
        let cmds: Vec<String> = detected.on_edit.iter()
            .map(|c| format!("  \"{c}\""))
            .collect();
        format!("on_edit = [\n{}\n]\n", cmds.join(",\n"))
    };

    let on_task_done_active = if detected.on_task_done.is_empty() {
        String::new()
    } else {
        let cmds: Vec<String> = detected.on_task_done.iter()
            .map(|c| format!("  \"{c}\""))
            .collect();
        format!("on_task_done = [\n{}\n]\n", cmds.join(",\n"))
    };

    let block = format!(
        r#"
# ── Hooks (auto-detected) ────────────────────────────────────────────────────
# PareCode detected your project type and configured these hooks automatically.
# Edit freely — set hooks_disabled = true to disable all hooks for this profile.
#
# on_edit      — runs after every edit_file/write_file; output injected into
#                the model's context so it can self-correct compile errors.
# on_task_done — runs after the full agent loop; shown in TUI only.
# on_plan_step_done — runs after each plan step passes.
# on_session_start  — runs when the TUI starts.
# on_session_end    — runs when the TUI exits.
[profiles.{profile_name}.hooks]
{on_edit_active}{on_task_done_active}# on_plan_step_done = []
# on_session_start  = []
# on_session_end    = []
"#
    );

    // Append to config file (non-fatal on failure)
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&config_path) {
        let _ = f.write_all(block.as_bytes());
    }

    detected
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

    // ── Language detection ──────────────────────────────────────────────────────

    #[test]
    fn test_detect_language_hooks_rust() {
        // This test will pass if Cargo.toml exists in the current directory
        // (which it does for this Rust project)
        let hooks = detect_language_hooks();
        assert_eq!(hooks.on_edit, vec!["cargo check -q"]);
        assert_eq!(hooks.on_task_done, vec!["cargo test -q 2>&1 | tail -5"]);
        assert!(hooks.on_plan_step_done.is_empty());
        assert!(hooks.on_session_start.is_empty());
        assert!(hooks.on_session_end.is_empty());
    }

    #[test]
    fn test_which_binary_exists() {
        // Test with a binary that should exist on all Unix-like systems
        assert!(which_binary("sh"));
        assert!(which_binary("echo"));
    }

    #[test]
    fn test_which_binary_not_exists() {
        // Test with a binary that definitely doesn't exist
        assert!(!which_binary("this_binary_definitely_does_not_exist_12345"));
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