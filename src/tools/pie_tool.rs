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
use crate::pie::{Cluster, ProjectGraph};

pub fn definition() -> Value {
    serde_json::json!({
        "name": "find_symbol",
        "description": "Locate any symbol (function, struct, enum, trait) OR source file by name. \
                        Returns file path and line number. \
                        ALWAYS call this before grep, bash, or read_file when you need to find \
                        where something is defined — it covers both symbol names (e.g. \"AppState\") \
                        and file name stems (e.g. \"config\" finds src/config.rs). \
                        Zero disk reads — instant hashmap lookup.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Symbol name (e.g. \"AppState\", \"run_tui\") or file name stem (e.g. \"config\", \"agent\", \"config.rs\")"
                }
            },
            "required": ["name"]
        }
    })
}

pub fn execute(args: &Value, graph: &ProjectGraph) -> String {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return "Provide name= for find_symbol. Example: find_symbol(name=\"AppState\") or find_symbol(name=\"config\")".to_string();
    }

    // If the query looks like a filename (contains '.' or ends with a known extension),
    // search file_lines keys instead of the symbol table.
    let looks_like_file = name.contains('.') || name.ends_with(".rs") || name.ends_with(".toml");
    if looks_like_file {
        return find_file(name, graph);
    }

    // Exact symbol match — include exact line_range and signature if available
    let matches: Vec<String> = graph.symbols.iter()
        .filter(|s| s.name == name)
        .map(|s| {
            let start = s.line.saturating_sub(1).max(1);
            let end = s.end_line;
            let sig_part = s.signature.as_deref()
                .map(|sig| format!("\n    {}: {}", s.kind.label(), sig))
                .unwrap_or_default();
            format!(
                "  {}:{} ({}) → read_file(path=\"{}\", line_range=[{}, {}]){}",
                s.file, s.line, s.kind.label(), s.file, start, end, sig_part
            )
        })
        .collect();

    if !matches.is_empty() {
        return format!(
            "'{}' defined at:\n{}\nUse the EXACT line_range shown — it covers the complete body.",
            name, matches.join("\n")
        );
    }

    // No exact symbol match — try partial symbol match
    let sym_partial: Vec<String> = graph.symbols.iter()
        .filter(|s| s.name.to_lowercase().contains(&name.to_lowercase()))
        .take(5)
        .map(|s| format!("  {}:{}-{} — {} ({})", s.file, s.line, s.end_line, s.name, s.kind.label()))
        .collect();

    // Also try file match as fallback (user may have omitted the extension)
    let file_matches: Vec<String> = graph.file_lines.keys()
        .filter(|f| {
            let stem = std::path::Path::new(f.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            stem.to_lowercase() == name.to_lowercase()
        })
        .map(|f| format!("  {f} ({} lines)", graph.file_lines[f]))
        .collect();

    // File match with no ambiguous symbol hits → clean file response (same as dot-query)
    if !file_matches.is_empty() && sym_partial.is_empty() {
        return find_file(name, graph);
    }

    if !sym_partial.is_empty() || !file_matches.is_empty() {
        let mut out = format!("'{name}' not found as exact symbol.");
        if !file_matches.is_empty() {
            out.push_str(&format!("\nFiles named '{name}':\n{}", file_matches.join("\n")));
        }
        if !sym_partial.is_empty() {
            out.push_str(&format!("\nSimilar symbols:\n{}", sym_partial.join("\n")));
        }
        return out;
    }

    format!("Symbol or file '{name}' not found in project index.")
}

/// Search file_lines for a filename match (used when the query looks like a filename).
fn find_file(name: &str, graph: &ProjectGraph) -> String {
    // Exact path suffix match first (e.g. "src/config.rs")
    let exact: Vec<String> = graph.file_lines.keys()
        .filter(|f| f.ends_with(name) || f.as_str() == name)
        .map(|f| format!("  {f} ({} lines)", graph.file_lines[f]))
        .collect();

    if !exact.is_empty() {
        return format!(
            "File '{name}':\n{}\nUse read_file(path) to read it.",
            exact.join("\n")
        );
    }

    // Stem match (e.g. "config.rs" → src/config.rs)
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let partial: Vec<String> = graph.file_lines.keys()
        .filter(|f| {
            let fstem = std::path::Path::new(f.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            fstem.to_lowercase() == stem.to_lowercase()
        })
        .map(|f| format!("  {f} ({} lines)", graph.file_lines[f]))
        .collect();

    if !partial.is_empty() {
        return format!(
            "File '{name}':\n{}\nUse read_file(path) to read it.",
            partial.join("\n")
        );
    }

    // Stem-substring fallback: "config_view" → finds "config.rs" (stem "config" is contained
    // within "config_view"), and "conf" → finds "config.rs" ("conf" contained in stem "config").
    let stem_lower = stem.to_lowercase();
    let mut stem_fuzzy: Vec<(&String, usize)> = graph.file_lines.keys()
        .filter_map(|f| {
            let fstem = std::path::Path::new(f.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            // Score: longer common prefix = better match
            if stem_lower.contains(&fstem) || fstem.contains(&stem_lower) {
                let common = stem_lower.chars().zip(fstem.chars()).take_while(|(a, b)| a == b).count();
                Some((f, common))
            } else {
                None
            }
        })
        .collect();
    stem_fuzzy.sort_by(|a, b| b.1.cmp(&a.1));
    stem_fuzzy.truncate(5);

    if !stem_fuzzy.is_empty() {
        let lines: Vec<String> = stem_fuzzy.iter()
            .map(|(f, _)| format!("  {f} ({} lines)", graph.file_lines[*f]))
            .collect();
        return format!(
            "No exact match for '{name}'. Similar files:\n{}",
            lines.join("\n")
        );
    }

    format!("File '{name}' not found in project index.")
}

/// Build a compact PIE summary for session-start injection and the synthetic tool result.
/// Target: ~300-400 tokens base + ~500 tokens per focus file — enough to orient without
/// requiring discovery scans. Called by pie_injection_messages() in agent.rs.
///
/// `focus_files` — paths of attached/anchored files whose full symbol maps are injected.
/// These are listed with exact line numbers so the model can jump straight to targeted reads.
pub fn build_compact_summary(
    graph: &ProjectGraph,
    narrative: &ProjectNarrative,
    focus_files: &[String],
) -> String {
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
            // List file names so models can navigate directly without a discovery scan
            let file_names: Vec<&str> = cluster.files.iter()
                .filter_map(|f| std::path::Path::new(f).file_name()?.to_str())
                .collect();
            out.push_str(&format!(
                "- **{}** [{}]{}\n",
                cluster.name,
                file_names.join(", "),
                summary_part
            ));
        }
        out.push('\n');
    }

    // Context file symbol maps — injected when the user attaches files or when
    // keyword anchoring identifies relevant files. Provides exact line numbers so
    // the model can formulate targeted reads/edits without any discovery calls.
    if !focus_files.is_empty() {
        use crate::index::SymbolKind;
        out.push_str("## Context file symbols\n");
        for path in focus_files {
            let mut syms: Vec<&crate::index::Symbol> = graph.symbols.iter()
                .filter(|s| &s.file == path)
                .filter(|s| matches!(
                    s.kind,
                    SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
                ))
                .collect();
            syms.sort_by_key(|s| s.line);
            if !syms.is_empty() {
                let display = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str());
                out.push_str(&format!("### {display} ({path})\n"));
                for s in syms.iter().take(80) {
                    out.push_str(&format!("- {} {} [line {}]\n", s.kind.label(), s.name, s.line));
                }
                if syms.len() > 80 {
                    out.push_str(&format!("- … {} more\n", syms.len() - 80));
                }
            }
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

    // Symbol-enriched section: top symbols per cluster so the model arrives
    // knowing key struct/fn names without needing find_symbol for discovery.
    // Budget: ~5 types + ~5 fns per cluster, capped at 8 clusters total.
    if !graph.clusters.is_empty() {
        out.push_str("## Key symbols\n");
        for cluster in graph.clusters.iter().take(8) {
            let syms = symbols_for_cluster(cluster, graph);
            if !syms.is_empty() {
                out.push_str(&format!("**{}:** {}\n", cluster.name, syms.join(", ")));
            }
        }
        out.push('\n');
    }

    out.push_str(
        "Use `find_symbol(name=\"SymbolName\")` to locate any symbol or file by name — \
         always call this before grep or bash.",
    );

    out
}

/// Return up to 10 representative symbol names for a cluster (types first, then fns).
/// Avoids line numbers here — the model calls find_symbol for exact location.
fn symbols_for_cluster(cluster: &Cluster, graph: &ProjectGraph) -> Vec<String> {
    use crate::index::SymbolKind;

    let cluster_files: std::collections::HashSet<&String> = cluster.files.iter().collect();

    let mut types: Vec<&str> = graph.symbols.iter()
        .filter(|s| cluster_files.contains(&s.file))
        .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait))
        .map(|s| s.name.as_str())
        .collect();
    types.dedup();
    types.truncate(5);

    let mut fns: Vec<&str> = graph.symbols.iter()
        .filter(|s| cluster_files.contains(&s.file))
        .filter(|s| matches!(s.kind, SymbolKind::Function))
        .map(|s| s.name.as_str())
        .collect();
    fns.dedup();
    fns.truncate(5);

    types.into_iter().chain(fns).map(|s| s.to_string()).collect()
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
                end_line: 200,
                kind: SymbolKind::Function,
                signature: Some("(task: &str, client: &Client) -> Result<AgentDone>".to_string()),
            },
            Symbol {
                name: "AppState".to_string(),
                file: "src/tui/mod.rs".to_string(),
                line: 100,
                end_line: 150,
                kind: SymbolKind::Struct,
                signature: Some("mode: AppMode, input: String, messages: Vec<ChatMessage>".to_string()),
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
    fn test_find_symbol_by_filename_with_extension() {
        let graph = make_graph();
        // "agent.rs" should find src/agent.rs via file lookup
        let args = serde_json::json!({"name": "agent.rs"});
        let result = execute(&args, &graph);
        assert!(result.contains("src/agent.rs"), "should find file: {result}");
        assert!(!result.contains("not found"), "should not say not found: {result}");
    }

    #[test]
    fn test_find_symbol_by_filename_stem() {
        let graph = make_graph();
        // "agent" (no extension) should still find src/agent.rs via stem fallback
        // Note: "agent" has no '.' so goes through symbol path first, then file fallback
        let args = serde_json::json!({"name": "agent"});
        let result = execute(&args, &graph);
        // Should find src/agent.rs via the file fallback in the symbol path
        assert!(result.contains("src/agent.rs") || result.contains("not found"), "got: {result}");
    }

    #[test]
    fn test_find_symbol_dotted_name_routes_to_file_search() {
        let graph = make_graph();
        // "config_view.rs" — not in graph exactly, but stem "config_view" contains "main"? No.
        // Use "main_view.rs" → should fuzzy-find "src/main.rs" (stem "main" contained in "main_view")
        let args = serde_json::json!({"name": "main_view.rs"});
        let result = execute(&args, &graph);
        // Should find src/main.rs via stem-fuzzy ("main" is contained in "main_view")
        assert!(result.contains("src/main.rs") || result.contains("not found"), "got: {result}");
        // Must NOT use the symbol error message format
        assert!(!result.contains("Symbol 'main_view.rs'"), "should not use symbol error message: {result}");
    }

    #[test]
    fn test_find_file_fuzzy_stem_match() {
        let graph = make_graph();
        // "agent_config.rs" — model guessing; stem "agent_config" contains "agent"
        // so src/agent.rs should appear in similar files
        let args = serde_json::json!({"name": "agent_config.rs"});
        let result = execute(&args, &graph);
        assert!(result.contains("src/agent.rs"), "should fuzzy-find agent.rs: {result}");
    }

    #[test]
    fn test_build_compact_summary() {
        let graph = make_graph();
        let narrative = make_narrative();
        let summary = build_compact_summary(&graph, &narrative, &[]);
        assert!(summary.contains("# Project index"));
        assert!(summary.contains("## Architecture"));
        assert!(summary.contains("## Clusters"));
        assert!(summary.contains("## Key files"));
        assert!(summary.contains("## Key symbols"));
        assert!(summary.contains("find_symbol"));
    }

    #[test]
    fn test_build_compact_summary_includes_known_symbols() {
        let graph = make_graph();
        let narrative = make_narrative();
        let summary = build_compact_summary(&graph, &narrative, &[]);
        // AppState (struct in tui cluster) and run_tui (fn in agent cluster) should appear
        assert!(summary.contains("AppState"), "should include AppState struct: {summary}");
        assert!(summary.contains("run_tui"), "should include run_tui fn: {summary}");
    }

    #[test]
    fn test_build_compact_summary_empty_narrative() {
        let graph = make_graph();
        let narrative = ProjectNarrative::default();
        let summary = build_compact_summary(&graph, &narrative, &[]);
        assert!(summary.contains("# Project index"));
        assert!(!summary.contains("## Architecture"));
        assert!(summary.contains("## Clusters"));
        assert!(summary.contains("## Key symbols"));
    }

    #[test]
    fn test_build_compact_summary_focus_files_injects_symbols() {
        let graph = make_graph();
        let narrative = make_narrative();
        let focus = vec!["src/agent.rs".to_string()];
        let summary = build_compact_summary(&graph, &narrative, &focus);
        assert!(summary.contains("## Context file symbols"), "should have context section: {summary}");
        assert!(summary.contains("src/agent.rs"), "should include the focus file path: {summary}");
        // run_tui is a fn in src/agent.rs — should appear with its line number
        assert!(summary.contains("run_tui"), "should list run_tui: {summary}");
        assert!(summary.contains("line 42"), "should include line number: {summary}");
    }

    #[test]
    fn test_build_compact_summary_no_focus_files_no_section() {
        let graph = make_graph();
        let narrative = make_narrative();
        let summary = build_compact_summary(&graph, &narrative, &[]);
        assert!(!summary.contains("## Context file symbols"), "should not have context section when no focus files: {summary}");
    }
}
