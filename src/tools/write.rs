use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "write_file",
        "description": "Write content to a new file. For existing files use edit_file instead. Pass overwrite=true only to intentionally replace an entire file.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Set true to overwrite an existing file (default: false)"
                }
            },
            "required": ["path", "content"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"].as_str().context("write_file: missing 'path'")?;
    let content = args["content"]
        .as_str()
        .context("write_file: missing 'content'")?;
    let overwrite = args["overwrite"].as_bool().unwrap_or(false);

    // Guard: refuse to silently overwrite existing files
    if Path::new(path).exists() && !overwrite {
        return Ok(format!(
            "'{path}' already exists — use edit_file to modify it, or pass overwrite=true to replace it entirely"
        ));
    }

    // Create parent directories if needed
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("write_file: cannot create dirs for '{path}'"))?;
        }
    }

    let line_count = content.lines().count();
    fs::write(path, content)
        .with_context(|| format!("write_file: cannot write '{path}'"))?;

    Ok(format!("✓ Wrote {path} ({line_count} lines)"))
}
