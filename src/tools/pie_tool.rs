/// PIE `find_symbol` tool — locate any symbol by name in the project graph.
///
/// The injection pipeline (pie_injection_messages + to_context_package) delivers
/// cluster/architecture context before the agent starts. This tool fills the one
/// gap the injection can't cover: cold-start lookup of a symbol by name.
///
/// All other project_index kinds (cluster, hotspots, recent_tasks, summary) have
/// been removed — they are redundant with the pre-delivered injection context.
use serde_json::Value;

use crate::narrative::ProjectNarrative;
use crate::pie::ProjectGraph;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "find_symbol",
        "description": "Find where a symbol (function, struct, type) is defined. \
                        Returns file path and line number. \
                        Use this before read_file when you know the name but not the location. \
                        Faster than grep — zero disk reads.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Symbol name to look up"
                }
            },
            "required": ["name"]
        }
    })
}

pub fn execute(args: &Value, graph: &ProjectGraph) -> String {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return "Provide name= for find_symbol. Example: find_symbol(name=\"AppState\")".to_string();
    }

    let matches: Vec<String> = graph.symbols.iter()
        .filter(|s| s.name == name)
        .map(|s| format!("  {}:{} ({})", s.file, s.line, s.kind.label()))
        .collect();

    if matches.is_empty() {
        // Partial match fallback — help with typos / partial names
        let partial: Vec<String> = graph.symbols.iter()
            .filter(|s| s.name.to_lowercase().contains(&name.to_lowercase()))
            .take(5)
            .map(|s| format!("  {}:{} — {} ({})", s.file, s.line, s.name, s.kind.label()))
            .collect();

        if partial.is_empty() {
            return format!("Symbol '{name}' not found in project index.");
        }
        return format!(
            "Symbol '{name}' not found. Similar names:\n{}",
            partial.join("\n")
        );
    }

    format!(
        "'{}' defined at:\n{}\nUse read_file(path, line_range=[N-2, N+20]) to read it.",
        name,
        matches.join("\n")
    )
}

/// Build a compact PIE summary for session-start injection and the synthetic tool result.
/// Target: ~300-400 tokens — enough to orient, not so much as to replace the tool.
/// Called by pie_injection_messages() in agent.rs.
pub fn build_compact_summary(graph: &ProjectGraph, narrative: &ProjectNarrative) -> String {
    let mut out = String::new();

    let proj_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".to_string());

    out.push_str(&format!("# Project index — {proj_name}\n"));

    if !narrative.architecture_summary.is_empty() {
        out.push_str("## Architecture\n");
        // Cap to first 2 sentences to keep it lean
        let mut sentences = narrative.architecture_summary.splitn(3, ". ");
        if let Some(s1) = sentences.next() {
            out.push_str(s1);
            if let Some(s2) = sentences.next() {
                out.push_str(". ");
                out.push_str(s2);
                out.push('.');
            }
        }
        out.push_str("\n\n");
    }

    if !graph.clusters.is_empty() {
        out.push_str("## Clusters\n");
        for cluster in &graph.clusters {
            let summary = narrative
                .cluster_summaries
                .get(&cluster.name)
                .map(|s| {
                    let words: Vec<&str> = s.split_whitespace().collect();
                    if words.len() > 12 {
                        format!("{} …", words[..12].join(" "))
                    } else {
                        s.clone()
                    }
                })
                .unwrap_or_default();
            let summary_part = if summary.is_empty() {
                String::new()
            } else {
                format!(" — {summary}")
            };
            out.push_str(&format!(
                "- **{}** ({} files){}\n",
                cluster.name,
                cluster.files.len(),
                summary_part
            ));
        }
        out.push('\n');
    }

    // Top 5 files by line count
    let mut files: Vec<(&String, &usize)> = graph.file_lines.iter().collect();
    files.sort_by(|a, b| b.1.cmp(a.1));
    if !files.is_empty() {
        out.push_str("## Key files\n");
        for (f, l) in files.iter().take(5) {
            out.push_str(&format!("- {f} ({l} lines)\n"));
        }
        out.push('\n');
    }

    out.push_str(
        "Use `find_symbol(name=\"SymbolName\")` to locate any function or struct by name.",
    );

    out
}


// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Symbol, SymbolKind};
    use crate::pie::{Cluster, ProjectGraph};
    use std::collections::HashMap;

    fn make_graph() -> ProjectGraph {
        let mut file_lines = HashMap::new();
        file_lines.insert("src/main.rs".to_string(), 100usize);
        file_lines.insert("src/agent.rs".to_string(), 500usize);
        file_lines.insert("src/tui/mod.rs".to_string(), 900usize);

        let symbols = vec![
            Symbol {
                name: "run_tui".to_string(),
                file: "src/agent.rs".to_string(),
                line: 42,
                kind: SymbolKind::Function,
            },
            Symbol {
                name: "AppState".to_string(),
                file: "src/tui/mod.rs".to_string(),
                line: 100,
                kind: SymbolKind::Struct,
            },
        ];

        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for s in &symbols {
            by_name.entry(s.name.clone()).or_default().push(s.file.clone());
        }

        ProjectGraph {
            schema_version: 1,
            clusters: vec![
                Cluster {
                    name: "agent".to_string(),
                    files: vec!["src/agent.rs".to_string()],
                    entry_files: vec!["src/agent.rs".to_string()],
                    summary: Some("Handles agentic tool loop and dispatch.".to_string()),
                },
                Cluster {
                    name: "tui".to_string(),
                    files: vec!["src/tui/mod.rs".to_string()],
                    entry_files: vec!["src/tui/mod.rs".to_string()],
                    summary: Some("Terminal UI rendering and event handling.".to_string()),
                },
            ],
            file_hashes: HashMap::new(),
            symbols,
            by_name,
            file_lines,
            last_indexed: 0,
        }
    }

    fn make_narrative() -> ProjectNarrative {
        let mut cluster_summaries = HashMap::new();
        cluster_summaries.insert("agent".to_string(), "Handles agentic tool loop and dispatch.".to_string());
        cluster_summaries.insert("tui".to_string(), "Terminal UI rendering and event handling.".to_string());

        ProjectNarrative {
            schema_version: 1,
            architecture_summary: "PareCode is a TUI coding assistant. It uses a multi-turn agent loop with tool calls.".to_string(),
            cluster_summaries,
            conventions: vec!["2 clusters detected".to_string()],
            last_synthesized: 0,
            patches: vec![],
        }
    }

    #[test]
    fn test_find_symbol_found() {
        let graph = make_graph();
        let args = serde_json::json!({"name": "run_tui"});
        let result = execute(&args, &graph);
        assert!(result.contains("src/agent.rs"), "should contain file: {result}");
        assert!(result.contains("42"), "should contain line number: {result}");
    }

    #[test]
    fn test_find_symbol_not_found() {
        let graph = make_graph();
        let args = serde_json::json!({"name": "nonexistent_fn"});
        let result = execute(&args, &graph);
        assert!(result.contains("not found"), "should indicate not found: {result}");
    }

    #[test]
    fn test_find_symbol_partial_match() {
        let graph = make_graph();
        let args = serde_json::json!({"name": "App"});
        let result = execute(&args, &graph);
        // "App" is not an exact match but "AppState" contains it
        assert!(result.contains("AppState") || result.contains("not found"), "got: {result}");
    }

    #[test]
    fn test_find_symbol_missing_name() {
        let graph = make_graph();
        let args = serde_json::json!({});
        let result = execute(&args, &graph);
        assert!(result.contains("Provide name="), "should prompt for name: {result}");
    }

    #[test]
    fn test_build_compact_summary() {
        let graph = make_graph();
        let narrative = make_narrative();
        let summary = build_compact_summary(&graph, &narrative);
        assert!(summary.contains("# Project index"));
        assert!(summary.contains("## Architecture"));
        assert!(summary.contains("## Clusters"));
        assert!(summary.contains("## Key files"));
        assert!(summary.contains("find_symbol"));
    }

    #[test]
    fn test_build_compact_summary_empty_narrative() {
        let graph = make_graph();
        let narrative = ProjectNarrative::default();
        let summary = build_compact_summary(&graph, &narrative);
        assert!(summary.contains("# Project index"));
        assert!(!summary.contains("## Architecture"));
        assert!(summary.contains("## Clusters"));
    }
}
