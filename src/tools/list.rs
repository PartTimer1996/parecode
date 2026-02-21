use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Max entries to list before truncating.
const MAX_ENTRIES: usize = 200;
/// Default max depth.
const DEFAULT_DEPTH: usize = 3;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "list_files",
        "description": "List directory contents as a tree. Ignores common noise dirs (node_modules, .git, target).",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (default: current directory)"
                },
                "depth": {
                    "type": "integer",
                    "description": "Max depth to traverse (default: 3)"
                }
            },
            "required": []
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let root = args["path"].as_str().unwrap_or(".");
    let max_depth = args["depth"].as_u64().unwrap_or(DEFAULT_DEPTH as u64) as usize;

    let mut out = String::new();
    let mut count = 0;
    let mut truncated = false;

    walk(
        Path::new(root),
        0,
        max_depth,
        "",
        &mut out,
        &mut count,
        &mut truncated,
    );

    if truncated {
        out.push_str(&format!(
            "\n[Truncated at {MAX_ENTRIES} entries — use a more specific path or smaller depth]"
        ));
    } else {
        out.push_str(&format!("\n[{count} entries]"));
    }

    Ok(out)
}

static IGNORED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    ".next",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".cache",
    "coverage",
];

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    prefix: &str,
    out: &mut String,
    count: &mut usize,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| {
        // Dirs first, then files, both alphabetical
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        (is_file as u8, e.file_name())
    });

    let len = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        if *truncated {
            return;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let is_last = i == len - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let extension = if is_last { "    " } else { "│   " };

        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let display = if is_dir {
            format!("{}/", name_str)
        } else {
            name_str.to_string()
        };

        out.push_str(&format!("{}{}{}\n", prefix, connector, display));
        *count += 1;

        if *count >= MAX_ENTRIES {
            *truncated = true;
            return;
        }

        if is_dir && depth < max_depth {
            // Skip noise directories
            if IGNORED_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            let new_prefix = format!("{}{}", prefix, extension);
            walk(
                &entry.path(),
                depth + 1,
                max_depth,
                &new_prefix,
                out,
                count,
                truncated,
            );
        }
    }
}
