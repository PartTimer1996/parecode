use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "write_file",
        "description": "Create a NEW file that does not exist yet. NEVER use this on existing files — use edit_file instead. Passing overwrite=true on an existing file will be blocked if content is shorter than the original.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path for the new file"
                },
                "content": {
                    "type": "string",
                    "description": "Full content to write"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Only set true when intentionally replacing an entire existing file with complete content"
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

    let file_exists = Path::new(path).exists();

    // Guard 1: refuse to silently overwrite existing files without the flag
    if file_exists && !overwrite {
        return Ok(format!(
            "'{path}' already exists — use edit_file to modify it, or pass overwrite=true to replace it entirely"
        ));
    }

    // Guard 2: content-preservation check — if overwriting an existing file,
    // verify the new content isn't dramatically shorter than what's already there.
    // A new file being "written" that is much shorter than the existing one is
    // almost always a model that read a file, then wrote back an incomplete version.
    if file_exists && overwrite {
        if let Ok(existing) = fs::read_to_string(path) {
            let existing_lines = existing.lines().count();
            let new_lines = content.lines().count();
            // Refuse if new content loses more than 30% of lines vs the existing file,
            // unless the file was already tiny (< 10 lines) or the new content is larger.
            if existing_lines >= 10 && new_lines < existing_lines * 7 / 10 {
                return Ok(format!(
                    "Blocked: '{path}' has {existing_lines} lines but new content has only {new_lines} lines — \
                     this would delete {del} lines of existing content. \
                     Use edit_file to add or modify specific sections instead of replacing the file. \
                     If you genuinely need to replace the entire file, first use read_file to confirm you have the complete contents.",
                    del = existing_lines.saturating_sub(new_lines),
                ));
            }
        }
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
