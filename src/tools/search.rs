use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

/// Max lines of rg output to return inline.
const MAX_OUTPUT_LINES: usize = 30;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "search",
        "description": "Search for a pattern across files using ripgrep.\n\nCall project_index first — it has exact symbol locations with zero disk reads. Use search only for:\n- Finding all call sites of a function across multiple files\n- Confirming a pattern was fully removed after a replacement task\n- Regex patterns not covered by the project index\n\nDO NOT USE for locating symbols — project_index already has exact file and line numbers.",
        "parameters": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern"
                },
                "path": {
                    "type": "string",
                    "description": "Dir or file to search (default: .)"
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob filter, e.g. '*.ts', '*.rs'"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context around each match (default: 0). Keep at 0 unless you need surrounding code — context multiplies output size."
                }
            },
            "required": ["pattern"]
        }
    })
}

pub async fn execute(args: &Value) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .context("search: missing 'pattern'")?;
    let path = args["path"].as_str().unwrap_or(".");
    let context_lines = args["context_lines"].as_u64().unwrap_or(0);

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

    let output = cmd.output().await;

    // rg may not be installed — fall back to grep
    let output = match output {
        Ok(o) => o,
        Err(_) => {
            Command::new("grep")
                .arg("-rn")
                .arg(format!("-{context_lines}"))
                .arg(pattern)
                .arg(path)
                .output()
                .await
                .context("search: neither 'rg' nor 'grep' available")?
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg exit 2 = error (bad regex, path not found, etc.) — report it so the model
    // doesn't mistake an error for "no matches found"
    if output.status.code() == Some(2) || (stdout.trim().is_empty() && !stderr.trim().is_empty()) {
        return Ok(format!("[search error: {}]", stderr.trim()));
    }

    if stdout.trim().is_empty() {
        return Ok(format!("No matches for '{pattern}' in {path}."));
    }

    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();

    if total <= MAX_OUTPUT_LINES {
        return Ok(format!("[{total} lines matched]\n{stdout}"));
    }

    let truncated: String = lines[..MAX_OUTPUT_LINES].join("\n");
    Ok(format!(
        "[Showing {MAX_OUTPUT_LINES} of {total} result lines — use file_pattern or path to narrow]\n{truncated}"
    ))
}
