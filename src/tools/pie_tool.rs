/// PIE `project_index` tool — query the pre-built project graph and narrative.
///
/// Provides surgical access to symbol locations, cluster details, hotspots,
/// and task history. Call this before `search` or `read_file` when locating
/// symbols or understanding project structure.
use anyhow::Result;
use serde_json::Value;

use crate::narrative::ProjectNarrative;
use crate::pie::ProjectGraph;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "project_index",
        "description": "Query the pre-built project index. Call this BEFORE search or read_file \
                        when you need to locate a symbol, understand a cluster, or find relevant \
                        past tasks. Zero disk reads for symbol/cluster queries.\n\
                        \n\
                        kind options:\n\
                        - \"symbols\" + file: all symbols in a file with line numbers\n\
                        - \"cluster\" + cluster: full detail on one cluster (files, symbols, summary)\n\
                        - \"hotspots\": largest/most complex files by line count\n\
                        - \"recent_tasks\": last N completed task summaries with files modified\n\
                        - \"summary\": compact architecture overview (same as session-start context)\n\
                        \n\
                        Use kind=\"cluster\" to get the full symbol list for a cluster you saw \
                        in the session-start summary. Use kind=\"symbols\" to get exact line \
                        numbers for every function in a specific file.",
        "parameters": {
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["symbols", "cluster", "hotspots", "recent_tasks", "summary"],
                    "description": "What to query"
                },
                "file": {
                    "type": "string",
                    "description": "For kind=symbols: the relative file path"
                },
                "cluster": {
                    "type": "string",
                    "description": "For kind=cluster: cluster name from the session-start summary"
                },
                "limit": {
                    "type": "integer",
                    "description": "For kind=recent_tasks: max results (default 5)"
                }
            },
            "required": ["kind"]
        }
    })
}

pub fn execute(args: &Value, graph: &ProjectGraph, narrative: &ProjectNarrative) -> Result<String> {
    match args["kind"].as_str().unwrap_or("summary") {
        "symbols"      => Ok(execute_symbols(args, graph)),
        "cluster"      => Ok(execute_cluster(args, graph, narrative)),
        "hotspots"     => Ok(execute_hotspots(graph)),
        "recent_tasks" => Ok(execute_recent_tasks(args)),
        "summary"      => Ok(build_compact_summary(graph, narrative)),
        other          => Ok(format!(
            "Unknown kind: '{other}'. Use one of: symbols, cluster, hotspots, recent_tasks, summary"
        )),
    }
}

fn execute_symbols(args: &Value, graph: &ProjectGraph) -> String {
    let file = args["file"].as_str().unwrap_or("");
    if file.is_empty() {
        return "Provide file= for kind=symbols. Example: project_index(kind=\"symbols\", file=\"src/agent.rs\")".to_string();
    }
    let syms: Vec<String> = graph
        .symbols
        .iter()
        .filter(|s| s.file == file)
        .map(|s| format!("line {:4}: {} {}", s.line, s.kind.label(), s.name))
        .collect();
    if syms.is_empty() {
        if graph.file_lines.contains_key(file) {
            return format!("No symbols indexed in {file} (file exists but has no indexed symbols)");
        }
        let available: Vec<&str> = graph.file_lines.keys().map(|k| k.as_str()).take(10).collect();
        return format!(
            "File '{file}' not in graph index.\nAvailable files (first 10):\n{}",
            available.join("\n")
        );
    }
    format!("// {file}\n{}", syms.join("\n"))
}

fn execute_cluster(args: &Value, graph: &ProjectGraph, narrative: &ProjectNarrative) -> String {
    let name = args["cluster"].as_str().unwrap_or("");
    let cluster = match graph.clusters.iter().find(|c| c.name == name) {
        Some(c) => c,
        None => {
            let names: Vec<&str> = graph.clusters.iter().map(|c| c.name.as_str()).collect();
            return format!(
                "Cluster '{name}' not found. Available clusters: {}",
                names.join(", ")
            );
        }
    };

    let mut out = format!("## Cluster: {} ({} files)\n", cluster.name, cluster.files.len());

    if let Some(summary) = narrative.cluster_summaries.get(name) {
        out.push_str(summary);
        out.push('\n');
    }

    out.push_str("\n### Files\n");
    for f in &cluster.files {
        let lines = graph.file_lines.get(f).copied().unwrap_or(0);
        out.push_str(&format!("  {f} ({lines} lines)\n"));
    }

    let syms: Vec<String> = graph
        .symbols
        .iter()
        .filter(|s| cluster.files.contains(&s.file))
        .map(|s| format!("  line {:4}: {} {} [{}]", s.line, s.kind.label(), s.name, s.file))
        .collect();
    if !syms.is_empty() {
        out.push_str("\n### Symbols\n");
        out.push_str(&syms.join("\n"));
    }

    out
}

fn execute_hotspots(graph: &ProjectGraph) -> String {
    let mut files: Vec<(&String, &usize)> = graph.file_lines.iter().collect();
    files.sort_by(|a, b| b.1.cmp(a.1));

    let lines: Vec<String> = files
        .iter()
        .take(10)
        .map(|(f, l)| {
            let sym_count = graph.symbols.iter().filter(|s| &s.file == *f).count();
            format!("  {f} — {l} lines, {sym_count} symbols")
        })
        .collect();

    format!("## Largest files (complexity proxy)\n{}", lines.join("\n"))
}

fn execute_recent_tasks(args: &Value) -> String {
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;
    let tasks = crate::task_memory::load_recent(limit);
    if tasks.is_empty() {
        return "No task history yet.".to_string();
    }
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let files = if t.files_modified.is_empty() {
                String::new()
            } else {
                format!(" ({})", t.files_modified.join(", "))
            };
            format!("  [{}] {}{}", t.age_str(), t.summary, files)
        })
        .collect();
    format!("## Recent tasks\n{}", lines.join("\n"))
}

/// Build a compact PIE summary for session-start injection and kind=summary queries.
/// Target: ~300-400 tokens — enough to orient, not so much as to replace the tool.
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
        "Use `project_index(kind=\"cluster\", cluster=\"name\")` for full symbol lists, \
         or `project_index(kind=\"recent_tasks\")` for task history.",
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
                kind: SymbolKind::Struct,
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
    fn test_execute_symbols_found() {
        let graph = make_graph();
        let args = serde_json::json!({"kind": "symbols", "file": "src/agent.rs"});
        let result = execute_symbols(&args, &graph);
        assert!(result.contains("run_tui"), "should contain symbol name");
        assert!(result.contains("42"), "should contain line number");
    }

    #[test]
    fn test_execute_symbols_missing_file() {
        let graph = make_graph();
        let args = serde_json::json!({"kind": "symbols"});
        let result = execute_symbols(&args, &graph);
        assert!(result.contains("Provide file="));
    }

    #[test]
    fn test_execute_symbols_unknown_file() {
        let graph = make_graph();
        let args = serde_json::json!({"kind": "symbols", "file": "src/nonexistent.rs"});
        let result = execute_symbols(&args, &graph);
        assert!(result.contains("not in graph index"));
    }

    #[test]
    fn test_execute_cluster_found() {
        let graph = make_graph();
        let narrative = make_narrative();
        let args = serde_json::json!({"kind": "cluster", "cluster": "tui"});
        let result = execute_cluster(&args, &graph, &narrative);
        assert!(result.contains("tui"));
        assert!(result.contains("mod.rs"));
        assert!(result.contains("AppState"));
    }

    #[test]
    fn test_execute_cluster_not_found() {
        let graph = make_graph();
        let narrative = make_narrative();
        let args = serde_json::json!({"kind": "cluster", "cluster": "nonexistent"});
        let result = execute_cluster(&args, &graph, &narrative);
        assert!(result.contains("not found"));
        assert!(result.contains("agent") || result.contains("tui"));
    }

    #[test]
    fn test_execute_hotspots() {
        let graph = make_graph();
        let result = execute_hotspots(&graph);
        assert!(result.contains("tui/mod.rs"), "largest file should appear first");
        assert!(result.contains("900"));
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
        assert!(summary.contains("project_index"));
        // Should be lean — cap architecture at 2 sentences
        assert!(!summary.contains("PareCode is a TUI coding assistant. It uses a multi-turn agent loop with tool calls.\n\nPareCode"));
    }

    #[test]
    fn test_build_compact_summary_empty_narrative() {
        let graph = make_graph();
        let narrative = ProjectNarrative::default();
        let summary = build_compact_summary(&graph, &narrative);
        assert!(summary.contains("# Project index"));
        // No architecture section when empty
        assert!(!summary.contains("## Architecture"));
        // Still shows clusters
        assert!(summary.contains("## Clusters"));
    }

    #[test]
    fn test_execute_dispatch() {
        let graph = make_graph();
        let narrative = make_narrative();

        let args = serde_json::json!({"kind": "summary"});
        let result = execute(&args, &graph, &narrative).unwrap();
        assert!(result.contains("# Project index"));

        let args = serde_json::json!({"kind": "unknown_kind"});
        let result = execute(&args, &graph, &narrative).unwrap();
        assert!(result.contains("Unknown kind"));
    }
}
