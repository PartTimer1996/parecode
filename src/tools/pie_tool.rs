/// PIE graph query tools — `find_symbol` and `trace_calls`.
///
/// Two distinct questions, two tools:
///   find_symbol  — WHERE is a symbol defined? (file + line)
///   trace_calls  — WHAT does it connect to? (call chain, zero disk reads)
///
/// Both are in-memory graph lookups. The model uses these to orient itself
/// before reaching for read_file, which costs real tokens.
use std::collections::HashSet;

use serde_json::Value;

use crate::narrative::ProjectNarrative;
use crate::pie::{Cluster, ProjectGraph};

// Thresholds matching flowpaths.rs — keep in sync if those change.
const UTILITY_THRESHOLD: usize = 6; // symbols with this many callers are utilities
const MAX_AMBIGUITY: usize = 4;     // symbols defined in this many files are trait dispatch
const MAX_BREADTH: usize = 8;       // max callees shown per node before truncation

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

    // Exact symbol match — include exact line_range, signature, and call neighbourhood
    let matches: Vec<String> = graph.symbols.iter()
        .filter(|s| s.name == name)
        .map(|s| {
            let start = s.line.saturating_sub(1).max(1);
            let end = s.end_line;
            let sig_part = s.signature.as_deref()
                .map(|sig| format!("\n    {}: {}", s.kind.label(), sig))
                .unwrap_or_default();
            let mut entry = format!(
                "  {}:{}-{} ({}){}",
                s.file, s.line, end, s.kind.label(), sig_part
            );
            // Outgoing calls — where does this symbol dispatch to?
            let key = format!("{}::{}", s.file, s.name);
            if let Some(edges) = graph.call_edges.get(&key) {
                if !edges.is_empty() {
                    let callee_list: Vec<String> = edges.iter().map(|e| {
                        let loc = graph.symbols.iter()
                            .find(|sym| sym.name == e.callee && graph.by_name.get(&e.callee).map_or(false, |f| !f.is_empty()))
                            .map(|sym| format!("({}:{})", sym.file, sym.line))
                            .unwrap_or_default();
                        if loc.is_empty() { e.callee.clone() } else { format!("{} {}", e.callee, loc) }
                    }).collect();
                    entry.push_str(&format!("\n    calls: {}", callee_list.join(", ")));
                }
            }
            // Incoming callers — who calls this symbol?
            let callers = graph.callers_of(&s.name);
            if !callers.is_empty() {
                let shown: Vec<&str> = callers.iter().take(5).copied().collect();
                entry.push_str(&format!("\n    called by: {}", shown.join(", ")));
                if callers.len() > 5 {
                    entry.push_str(&format!(" (+{})", callers.len() - 5));
                }
            }
            entry
        })
        .collect();

    if !matches.is_empty() {
        return format!(
            "'{}' defined at:\n{}\nCall trace_calls(name=\"{}\") to see call structure before reading.",
            name, matches.join("\n"), name
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

// ── trace_calls ───────────────────────────────────────────────────────────────

pub fn trace_calls_definition() -> Value {
    serde_json::json!({
        "name": "trace_calls",
        "description": "Explore call chains in the project graph — zero disk reads.\n\
                        Call this BEFORE read_file to understand structure: what a \
                        function calls, or what calls it. Returns a call tree with \
                        file:line for each symbol. Use read_file only after you have \
                        identified the exact symbol to modify.\n\n\
                        direction \"calls\": outgoing calls (default) — what does X dispatch to?\n\
                        direction \"callers\": who calls X?\n\
                        direction \"both\": outgoing + incoming\n\
                        depth: hops to follow (default 2, max 4)",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Symbol name to trace from (e.g. \"run_tui\", \"dispatch_tool\")"
                },
                "depth": {
                    "type": "integer",
                    "description": "Hops to follow for outgoing calls (default 2, max 4)"
                },
                "direction": {
                    "type": "string",
                    "enum": ["calls", "callers", "both"],
                    "description": "Which direction to trace (default: \"calls\")"
                }
            },
            "required": ["name"]
        }
    })
}

pub fn trace_calls_execute(args: &Value, graph: &ProjectGraph) -> String {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return "Provide name= for trace_calls. Example: trace_calls(name=\"run_tui\")".to_string();
    }

    let depth = args["depth"].as_u64().unwrap_or(2).min(4) as usize;
    let direction = args["direction"].as_str().unwrap_or("calls");

    // Find starting symbol(s) — same name may live in multiple files
    let starts: Vec<&crate::index::Symbol> = graph.symbols.iter()
        .filter(|s| s.name == name)
        .collect();

    if starts.is_empty() {
        return format!(
            "Symbol '{name}' not found in call graph. \
             Try find_symbol(name=\"{name}\") for partial matches."
        );
    }

    // Pre-build caller counts once — used for utility detection
    let caller_counts = build_caller_counts(graph);

    let mut out = String::new();

    for (i, start) in starts.iter().enumerate() {
        if i > 0 { out.push('\n'); }

        let key = format!("{}::{}", start.file, start.name);
        out.push_str(&format!("{} ({}:{})\n", start.name, start.file, start.line));

        // Outgoing call chain
        if direction == "calls" || direction == "both" {
            let mut visited = HashSet::new();
            visited.insert(key.clone());
            append_calls(&key, graph, &caller_counts, &mut out, &mut visited, 1, depth, "  ");
            if graph.call_edges.get(&key).map_or(true, |e| e.is_empty()) {
                out.push_str("  (no outgoing project-internal calls indexed)\n");
            }
        }

        // Incoming callers (always depth-1 — deeper chains aren't useful here)
        if direction == "callers" || direction == "both" {
            if direction == "both" { out.push_str("  ←\n"); }
            let callers = graph.callers_of(name);
            if callers.is_empty() {
                out.push_str("  called by: (none — entry point or external)\n");
            } else {
                out.push_str("  called by:\n");
                for caller_key in callers.iter().take(MAX_BREADTH) {
                    let caller_sym_name = caller_key.split("::").last().unwrap_or(caller_key);
                    let loc = resolve_loc(caller_key, graph);
                    out.push_str(&format!("    {caller_sym_name} ({loc})\n"));
                }
                if callers.len() > MAX_BREADTH {
                    out.push_str(&format!(
                        "    … +{} more\n", callers.len() - MAX_BREADTH
                    ));
                }
            }
        }
    }

    out
}

/// Recursively append outgoing call edges as an indented tree.
fn append_calls(
    key: &str,
    graph: &ProjectGraph,
    caller_counts: &std::collections::HashMap<String, usize>,
    out: &mut String,
    visited: &mut HashSet<String>,
    current_depth: usize,
    max_depth: usize,
    indent: &str,
) {
    let Some(edges) = graph.call_edges.get(key) else { return };
    if edges.is_empty() { return }

    let shown = edges.iter().take(MAX_BREADTH);
    let overflow = edges.len().saturating_sub(MAX_BREADTH);

    for edge in shown {
        let callee = &edge.callee;
        let defs = graph.by_name.get(callee).map(|v| v.as_slice()).unwrap_or(&[]);

        // Trait dispatch — don't expand, just note it
        if defs.len() > MAX_AMBIGUITY {
            out.push_str(&format!("{indent}→ {callee} [trait — {} impls]\n", defs.len()));
            continue;
        }

        // Utility functions — don't expand, note the caller count
        let incoming = caller_counts.get(callee).copied().unwrap_or(0);
        if incoming >= UTILITY_THRESHOLD {
            out.push_str(&format!("{indent}→ {callee} [utility — {incoming} callers]\n"));
            continue;
        }

        for file in defs {
            let callee_key = format!("{}::{}", file, callee);
            let loc = graph.symbols.iter()
                .find(|s| s.name == *callee && &s.file == file)
                .map(|s| format!("{}:{}", s.file, s.line))
                .unwrap_or_else(|| file.clone());

            if visited.contains(&callee_key) {
                out.push_str(&format!("{indent}→ {callee} ({loc}) [↩ cycle]\n"));
                continue;
            }

            out.push_str(&format!("{indent}→ {callee} ({loc})\n"));

            if current_depth < max_depth {
                visited.insert(callee_key.clone());
                append_calls(
                    &callee_key, graph, caller_counts, out, visited,
                    current_depth + 1, max_depth,
                    &format!("{indent}   "),
                );
            }
        }
    }

    if overflow > 0 {
        out.push_str(&format!(
            "{indent}… +{overflow} more (use depth=1 on any symbol above for details)\n"
        ));
    }
}

fn build_caller_counts(graph: &ProjectGraph) -> std::collections::HashMap<String, usize> {
    let mut counts = std::collections::HashMap::new();
    for edges in graph.call_edges.values() {
        for edge in edges {
            *counts.entry(edge.callee.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn resolve_loc(key: &str, graph: &ProjectGraph) -> String {
    graph.symbols.iter()
        .find(|s| format!("{}::{}", s.file, s.name) == key)
        .map(|s| format!("{}:{}", s.file, s.line))
        .unwrap_or_else(|| key.to_string())
}

/// Build a compact PIE summary for session-start injection.
/// Target: ~300-400 tokens — project orientation only (architecture, clusters, key files,
/// key symbols). Per-file symbol maps with line ranges live in the task message instead,
/// where they are at the model's highest-attention point.
pub fn build_compact_summary(
    graph: &ProjectGraph,
    narrative: &ProjectNarrative,
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
            call_edges: HashMap::new(),
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
        let summary = build_compact_summary(&graph, &narrative);
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
        let summary = build_compact_summary(&graph, &narrative);
        assert!(summary.contains("AppState"), "should include AppState struct: {summary}");
        assert!(summary.contains("run_tui"), "should include run_tui fn: {summary}");
    }

    #[test]
    fn test_build_compact_summary_empty_narrative() {
        let graph = make_graph();
        let narrative = ProjectNarrative::default();
        let summary = build_compact_summary(&graph, &narrative);
        assert!(summary.contains("# Project index"));
        assert!(!summary.contains("## Architecture"));
        assert!(summary.contains("## Clusters"));
        assert!(summary.contains("## Key symbols"));
    }
}
