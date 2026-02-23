pub mod bash;
pub mod edit;
pub mod list;
pub mod patch;
pub mod read;
pub mod recall;
pub mod search;
pub mod write;

use anyhow::{anyhow, Result};
use crate::client::Tool;
use serde_json::Value;

/// All available tool definitions (sent to the model).
pub fn all_definitions() -> Vec<Tool> {
    vec![
        def(read::definition()),
        def(write::definition()),
        def(edit::definition()),
        def(patch::definition()),
        def(bash::definition()),
        def(search::definition()),
        def(list::definition()),
        def(recall::definition()),
    ]
}

fn def(v: Value) -> Tool {
    Tool {
        name: v["name"].as_str().unwrap_or("").to_string(),
        description: v["description"].as_str().unwrap_or("").to_string(),
        parameters: v["parameters"].clone(),
    }
}

/// Returns true if this is a built-in native tool (not an MCP tool).
pub fn is_native(name: &str) -> bool {
    matches!(name, "read_file" | "write_file" | "edit_file" | "patch_file" | "bash" | "search" | "list_files" | "recall")
}

/// Dispatch a synchronous tool call by name.
/// Note: "bash" and "recall" are handled asynchronously in agent.rs.
pub fn dispatch(name: &str, args: &Value) -> Result<String> {
    match name {
        "read_file"  => read::execute(args),
        "write_file" => write::execute(args),
        "edit_file"  => edit::execute(args),
        "patch_file" => patch::execute(args),
        "search"     => search::execute(args),
        "list_files" => list::execute(args),
        other        => Err(anyhow!("Unknown tool: '{other}'")),
    }
}
