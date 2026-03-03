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

// ─────────────────────────────────────────────────────────────────────────────
// Tool name constants — single source of truth
// ─────────────────────────────────────────────────────────────────────────────

pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_WRITE_FILE: &str = "write_file";
pub const TOOL_EDIT_FILE: &str = "edit_file";
pub const TOOL_PATCH_FILE: &str = "patch_file";
pub const TOOL_BASH: &str = "bash";
pub const TOOL_SEARCH: &str = "search";
pub const TOOL_LIST_FILES: &str = "list_files";
pub const TOOL_RECALL: &str = "recall";
pub const TOOL_ASK_USER: &str = "ask_user";

// Turn thresholds for phase-adaptive tool selection
const TURN_EXPLORATION_END: usize = 1;
const TURN_MUTATION_START: usize = 2;
const TURN_RECALL_USEFUL: usize = 3;

/// All tool names as a slice, useful for bulk operations.
pub fn all_tool_names() -> &'static [&'static str] {
    &[
        TOOL_READ_FILE,
        TOOL_WRITE_FILE,
        TOOL_EDIT_FILE,
        TOOL_PATCH_FILE,
        TOOL_BASH,
        TOOL_SEARCH,
        TOOL_LIST_FILES,
        TOOL_RECALL,
        TOOL_ASK_USER,
    ]
}

/// Phase thresholds for adaptive tool selection.
#[derive(Debug, Clone, Copy)]
pub struct TurnThresholds {
    pub exploration_end: usize,
    pub mutation_start: usize,
    pub recall_useful: usize,
}

impl Default for TurnThresholds {
    fn default() -> Self {
        Self {
            exploration_end: TURN_EXPLORATION_END,
            mutation_start: TURN_MUTATION_START,
            recall_useful: TURN_RECALL_USEFUL,
        }
    }
}

/// Dispatch to get a tool's definition by name.
pub fn get_tool(name: &str) -> Option<Value> {
    match name {
        TOOL_READ_FILE => Some(read::definition()),
        TOOL_WRITE_FILE => Some(write::definition()),
        TOOL_EDIT_FILE => Some(edit::definition()),
        TOOL_PATCH_FILE => Some(patch::definition()),
        TOOL_BASH => Some(bash::definition()),
        TOOL_SEARCH => Some(search::definition()),
        TOOL_LIST_FILES => Some(list::definition()),
        TOOL_RECALL => Some(recall::definition()),
        TOOL_ASK_USER => Some(ask::definition()),
        _ => None,
    }
}

/// All available tool definitions (sent to the model).
pub fn all_definitions() -> Vec<Tool> {
    all_tool_names()
        .iter()
        .filter_map(|&name| get_tool(name).map(def))
        .collect()
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
    let thresholds = TurnThresholds::default();
    let mut t = vec![
        def(ask::definition()),    // first: clarify before spiralling into reads
        def(read::definition()),
        def(edit::definition()),
        def(bash::definition()),
        def(search::definition()),
    ];

    // Exploration phase: navigation + file creation
    if turn <= thresholds.exploration_end {
        t.push(def(list::definition()));
        t.push(def(write::definition()));
        
    }

    // Mutation phase: multi-hunk diffs become useful after reading files
    if turn >= thresholds.mutation_start {
        t.push(def(patch::definition()));
    }

    // Recall is pointless until tool outputs have been summarised in history
    if history_has_summaries || turn >= thresholds.recall_useful {
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
    all_tool_names().contains(&name)
}

/// Dispatch a synchronous tool call by name.
/// Note: "bash" and "recall" are handled asynchronously in agent.rs.
pub fn dispatch(name: &str, args: &Value) -> Result<String> {
    // Static dispatch table built from single source of truth
    static TOOL_DISPATCH: &[(&str, fn(&Value) -> Result<String>)] = &[
        (TOOL_READ_FILE, read::execute),
        (TOOL_WRITE_FILE, write::execute),
        (TOOL_EDIT_FILE, edit::execute),
        (TOOL_PATCH_FILE, patch::execute),
        (TOOL_SEARCH, search::execute),
        (TOOL_LIST_FILES, list::execute),
        // (TOOL_BASH, bash::execute),
        // (TOOL_ASK_USER, ask::execute),
        // (TOOL_RECALL, recall::execute),
    ];

    TOOL_DISPATCH
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f(args))
        .ok_or_else(|| anyhow!("Unknown tool: '{}'", name))?
}
