use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// Max lines of output to return inline. Rest is truncated.
const MAX_OUTPUT_LINES: usize = 500;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "bash",
        "description": "Run a shell command. Use for: compiling, tests, git, package managers.\n\
                        DO NOT use grep/rg for symbol lookup — project_index has exact locations with zero disk reads. Only grep for call-site patterns across the codebase after you already know where the definition lives.\n\
                        DO NOT use for reading files — read_file provides hashes required for editing.",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Default: 30"
                }
            },
            "required": ["command"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<String> {
    let command = args["command"]
        .as_str()
        .context("bash: missing 'command'")?;

    let timeout_secs = Duration::from_secs(
        args["timeout_secs"].as_u64().unwrap_or(30)
    );

    let fut = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output();

    let output = match timeout(timeout_secs, fut).await {
        Ok(result) => result.with_context(|| format!("bash: failed to run '{command}'"))?,
        Err(_) => {
            return Ok(format!(
                "[exit code: -1]\n[timed out after {}s — command did not complete]",
                timeout_secs.as_secs()
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let mut result = String::new();

    if exit_code != 0 {
        result.push_str(&format!("[exit code: {exit_code}]\n"));
    }

    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n[stderr]\n{stderr}")
    };

    let lines: Vec<&str> = combined.lines().collect();
    let total = lines.len();

    if total == 0 {
        result.push_str("[no output]");
    } else if total <= MAX_OUTPUT_LINES {
        result.push_str(&combined);
    } else {
        for line in &lines[..MAX_OUTPUT_LINES] {
            result.push_str(line);
            result.push('\n');
        }
        result.push_str(&format!(
            "[+{} lines truncated — use a more specific command to reduce output]",
            total - MAX_OUTPUT_LINES
        ));
    }

    // When the model uses bash for navigation (pwd, ls, find, tree), nudge it
    // toward find_symbol which is faster, pre-indexed, and costs fewer tokens.
    let cmd_trim = command.trim();
    let is_nav = cmd_trim == "pwd"
        || cmd_trim.starts_with("ls")
        || cmd_trim.starts_with("find ")
        || cmd_trim.starts_with("tree")
        || cmd_trim.contains("&& ls")
        || cmd_trim.contains("; ls");
    if is_nav {
        result.push_str(
            "\n[Project is pre-indexed — use find_symbol(name=\"SymbolName\") to locate \
             any function or struct without disk reads.]"
        );
    }

    Ok(result)
}
