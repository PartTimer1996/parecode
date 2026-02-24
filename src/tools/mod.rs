pub mod ask;
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
        def(ask::definition()),
    ]
}

/// Phase-adaptive tool selection — only send tools relevant to the current turn.
///
/// Core tools (always): read_file, edit_file, bash, search, ask_user  (~940 tok)
/// Extended (conditional):
///   - list_files, write_file: early turns (exploration / creation)
///   - patch_file, recall: later turns (mutation / history retrieval)
///
/// Saves ~400-800 tokens/turn compared to sending all 9 tools every time.
pub fn tools_for_turn(turn: usize, history_has_summaries: bool) -> Vec<Tool> {
    let mut t = vec![
        def(read::definition()),
        def(edit::definition()),
        def(bash::definition()),
        def(search::definition()),
        def(ask::definition()),
    ];

    // Exploration phase: navigation + file creation
    if turn <= 1 {
        t.push(def(list::definition()));
        t.push(def(write::definition()));
    }

    // Mutation phase: multi-hunk diffs become useful after reading files
    if turn >= 2 {
        t.push(def(patch::definition()));
    }

    // Recall is pointless until tool outputs have been summarised in history
    if history_has_summaries || turn >= 3 {
        t.push(def(recall::definition()));
    }

    // write_file stays available after turn 1 if model previously used it
    // (handled by the `extra_tools` mechanism in the agent loop)

    t
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
    matches!(name, "read_file" | "write_file" | "edit_file" | "patch_file" | "bash" | "search" | "list_files" | "recall" | "ask_user")
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
