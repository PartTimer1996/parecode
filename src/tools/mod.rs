pub mod ask;
pub mod bash;
pub mod edit;
pub mod patch;
pub mod pie_tool;
pub mod read;
pub mod write;

use anyhow::{anyhow, Result};
use crate::client::Tool;
use serde_json::Value;

// ─────────────────────────────────────────────────────────────────────────────
// Tool name constants — single source of truth
// ─────────────────────────────────────────────────────────────────────────────

pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_READ_FILES: &str = "read_files";
pub const TOOL_WRITE_FILE: &str = "write_file";
pub const TOOL_EDIT_FILE: &str = "edit_file";
pub const TOOL_PATCH_FILE: &str = "patch_file";
pub const TOOL_BASH: &str = "bash";
pub const TOOL_ASK_USER: &str = "ask_user";
pub const TOOL_FIND_SYMBOL: &str = "find_symbol";
pub const TOOL_TRACE_CALLS: &str = "trace_calls";
pub const TOOL_CHECK_WIRING: &str = "check_wiring";
pub const TOOL_ORIENT: &str = "orient";

// Turn thresholds for phase-adaptive tool selection
const TURN_EXPLORATION_END: usize = 1;
const TURN_MUTATION_START: usize = 2;
    
/// All tool names as a slice, useful for bulk operations.
pub fn all_tool_names() -> &'static [&'static str] {
    &[
        TOOL_ORIENT,
        TOOL_FIND_SYMBOL,
        TOOL_TRACE_CALLS,
        TOOL_CHECK_WIRING,
        TOOL_READ_FILE,
        TOOL_READ_FILES,
        TOOL_WRITE_FILE,
        TOOL_EDIT_FILE,
        TOOL_PATCH_FILE,
        TOOL_BASH,
        TOOL_ASK_USER,
    ]
}

/// Phase thresholds for adaptive tool selection.
#[derive(Debug, Clone, Copy)]
pub struct TurnThresholds {
    pub exploration_end: usize,
    pub mutation_start: usize,
}

impl Default for TurnThresholds {
    fn default() -> Self {
        Self {
            exploration_end: TURN_EXPLORATION_END,
            mutation_start: TURN_MUTATION_START,
        }
    }
}

/// Dispatch to get a tool's definition by name.
pub fn get_tool(name: &str) -> Option<Value> {
    match name {
        TOOL_ORIENT => Some(pie_tool::orient_definition()),
        TOOL_FIND_SYMBOL => Some(pie_tool::definition()),
        TOOL_TRACE_CALLS => Some(pie_tool::trace_calls_definition()),
        TOOL_CHECK_WIRING => Some(pie_tool::check_wiring_definition()),
        TOOL_READ_FILE => Some(read::definition()),
        TOOL_READ_FILES => Some(pie_tool::read_files_definition()),
        TOOL_WRITE_FILE => Some(write::definition()),
        TOOL_EDIT_FILE => Some(edit::definition()),
        TOOL_PATCH_FILE => Some(patch::definition()),
        TOOL_BASH => Some(bash::definition()),
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
/// When `has_graph` is true, `orient` leads the list (replaces find_symbol + trace_calls).
/// orient returns struct signatures, locations, and call connections in one call.
///
/// When graph present: orient → check_wiring → read_files → edit_file → bash
/// When no graph:      read_file → edit_file → bash  (original behaviour)
///
/// Saves ~400-800 tokens/turn compared to sending all tools every turn.
pub fn tools_for_turn(turn: usize, has_graph: bool) -> Vec<Tool> {
    let thresholds = TurnThresholds::default();
    let mut t: Vec<Tool> = Vec::new();

    if has_graph {
        // Discovery-first ordering — position drives model behaviour.
        // orient + check_wiring are free (in-memory graph), read_files is batched discovery.
        // read_file is reinstated for pre-edit hash fetches — use it freely before/after edits.
        t.push(def(pie_tool::orient_definition()));
        t.push(def(pie_tool::check_wiring_definition()));
        t.push(def(pie_tool::read_files_definition()));
        t.push(def(read::definition()));
    } else {
        // No graph — single-file reads only
        t.push(def(read::definition()));
    }

    t.push(def(ask::definition()));
    t.push(def(edit::definition()));
    t.push(def(patch::definition()));
    t.push(def(bash::definition()));

    if turn <= thresholds.exploration_end {
        t.push(def(write::definition()));
    }

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
/// Note: "bash", "search", and "recall" are handled asynchronously in agent.rs.
pub fn dispatch(name: &str, args: &Value) -> Result<String> {
    // Static dispatch table built from single source of truth
    static TOOL_DISPATCH: &[(&str, fn(&Value) -> Result<String>)] = &[
        (TOOL_READ_FILE, read::execute),
        (TOOL_WRITE_FILE, write::execute),
        (TOOL_EDIT_FILE, edit::execute),
        (TOOL_PATCH_FILE, patch::execute),
        // (TOOL_BASH, bash::execute),    // async
        // (TOOL_ASK_USER, ask::execute), // async
    ];

    TOOL_DISPATCH
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f(args))
        .ok_or_else(|| anyhow!("Unknown tool: '{}'", name))?
}


// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_all_tool_names() {
        let names = all_tool_names();
        assert!(names.contains(&TOOL_READ_FILE));
        assert!(names.contains(&TOOL_WRITE_FILE));
        assert!(names.contains(&TOOL_EDIT_FILE));
        assert!(names.contains(&TOOL_PATCH_FILE));
        assert!(names.contains(&TOOL_BASH));
        assert!(names.contains(&TOOL_ASK_USER));
        assert!(names.contains(&TOOL_FIND_SYMBOL));
        assert!(names.contains(&TOOL_TRACE_CALLS));
        assert!(names.contains(&TOOL_CHECK_WIRING));
        assert!(names.contains(&TOOL_ORIENT));
        assert_eq!(names.len(), 11);
    }

    #[test]
    fn test_turn_thresholds_default() {
        let t = TurnThresholds::default();
        assert_eq!(t.exploration_end, 1);
        assert_eq!(t.mutation_start, 2);
    }

    #[test]
    fn test_get_tool() {
        assert!(get_tool(TOOL_READ_FILE).is_some());
        assert!(get_tool(TOOL_BASH).is_some());
        assert!(get_tool(TOOL_FIND_SYMBOL).is_some());
        assert!(get_tool(TOOL_ORIENT).is_some());
        assert!(get_tool("invalid_tool").is_none());

        let read_def = get_tool(TOOL_READ_FILE).unwrap();
        assert_eq!(read_def["name"], TOOL_READ_FILE);

        let orient_def = get_tool(TOOL_ORIENT).unwrap();
        assert_eq!(orient_def["name"], TOOL_ORIENT);
    }

    #[test]
    fn test_all_definitions() {
        let defs = all_definitions();
        assert_eq!(defs.len(), 11);
        assert!(defs.iter().any(|d| d.name == TOOL_READ_FILE));
        assert!(defs.iter().any(|d| d.name == TOOL_ASK_USER));
        assert!(defs.iter().any(|d| d.name == TOOL_ORIENT));
    }

    #[test]
    fn test_tools_for_turn_logic() {
        // Turn 0: Exploration (includes write + patch — patch always present now)
        let t0 = tools_for_turn(0, false);
        let names0: Vec<_> = t0.iter().map(|d| d.name.as_str()).collect();
        assert!(names0.contains(&TOOL_WRITE_FILE));
        assert!(names0.contains(&TOOL_PATCH_FILE));

        // Turn 2: Still has patch (write may be absent after exploration_end)
        let t2 = tools_for_turn(2, false);
        let names2: Vec<_> = t2.iter().map(|d| d.name.as_str()).collect();
        assert!(names2.contains(&TOOL_PATCH_FILE));
        assert!(!names2.contains(&TOOL_WRITE_FILE));
    }

    #[test]
    fn test_is_native() {
        assert!(is_native(TOOL_READ_FILE));
        assert!(is_native(TOOL_PATCH_FILE));
        assert!(!is_native("mcp_custom_tool"));
        assert!(!is_native(""));
    }

    #[test]
    fn test_dispatch_unknown_tool() {
        let res = dispatch("unknown", &json!({}));
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "Unknown tool: 'unknown'");
    }

    #[test]
    fn test_def_helper_edges() {
        // Test with missing fields or wrong types
        let d = def(json!({}));
        assert_eq!(d.name, "");
        assert_eq!(d.description, "");
        assert!(d.parameters.is_null());

        let d2 = def(json!({
            "name": "foo",
            "description": 123, // wrong type
            "parameters": {"type": "object"}
        }));
        assert_eq!(d2.name, "foo");
        assert_eq!(d2.description, ""); // defaults to empty string on non-str
        assert_eq!(d2.parameters["type"], "object");
    }
}
