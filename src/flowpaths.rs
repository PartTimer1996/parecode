/// Flow Path Tracer — pre-computed call chains for proactive context assembly.
///
/// Walks forward from entry points through the call graph (built by the
/// tree-sitter extractor in `callgraph.rs`), producing ordered chains of
/// symbols that represent the "things this project does."
///
/// These paths are the foundation for proactive context delivery: when a user
/// task matches a path, the entire relevant call chain is pre-loaded into the
/// model's first prompt — zero discovery tool calls needed.
///
/// # Storage
/// Persisted to `.parecode/paths.json` (separate from `project.graph` to keep
/// the graph file manageable). Rebuilt whenever call edges change.
///
/// # Entry point detection
/// Primary:  symbols with 0 incoming project-internal call edges
/// Secondary: symbols with ≤1 incoming edge AND ≥4 outgoing edges
///            (subsystem heads — e.g. `event_loop`, `run_tui`)
///
/// Test functions (`test_*`) are excluded.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::pie::ProjectGraph;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Symbols called by this many or more callers are considered shared utilities
/// (e.g. formatters, validators, loggers) and are not followed into paths.
const UTILITY_THRESHOLD: usize = 6;

/// Callees defined in this many or more files are trait-method dispatch and
/// are not followed (avoids exploding into every tool implementation, etc.).
const MAX_AMBIGUITY: usize = 4;

/// Maximum BFS steps per path — prevents runaway traversal on dense graphs.
const MAX_PATH_STEPS: usize = 18;

/// Paths shorter than this are discarded as uninteresting.
const MIN_PATH_STEPS: usize = 2;

/// Minimum outgoing edges for a symbol to qualify as a secondary entry point.
const SECONDARY_ENTRY_MIN_OUT: usize = 4;

/// Overlap fraction threshold for path deduplication (80%).
const DEDUP_OVERLAP: f32 = 0.80;

const PATHS_FILE: &str = ".parecode/paths.json";

// ── Public types ──────────────────────────────────────────────────────────────

/// A single pre-computed call chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowPath {
    /// Short unique id derived from entry + terminal symbol names.
    pub id: String,
    /// Human-readable label: `"entry_name → terminal_name"`.
    pub name: String,
    /// Caller key of the chain's starting symbol: `"src/main.rs::main"`.
    pub entry_key: String,
    /// Ordered BFS steps from the entry point (entry itself excluded).
    /// Each element is a caller key `"file::symbol_name"`.
    pub steps: Vec<String>,
    /// Domain keywords extracted from all symbol names in the path.
    /// Used for keyword-overlap matching against user task descriptions.
    pub keywords: Vec<String>,
}

impl FlowPath {
    /// All symbol keys in this path including the entry point.
    pub fn all_keys(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.entry_key.as_str())
            .chain(self.steps.iter().map(|s| s.as_str()))
    }
}

/// Result of matching a user task against the path index.
#[derive(Debug)]
pub enum PathMatch {
    /// One path scored significantly higher than all others.
    Clear(usize),
    /// 2–3 paths score similarly — caller should ask a narrowing question.
    Ambiguous(Vec<usize>),
    /// No path scored above threshold — fall back to current PIE behaviour.
    None,
}

/// The full set of paths for a project.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FlowPathIndex {
    pub paths: Vec<FlowPath>,
    /// Unix timestamp when this index was built.
    pub built_at: i64,
}

impl FlowPathIndex {
    // ── Build ─────────────────────────────────────────────────────────────────

    /// Build flow paths from a fully-populated `ProjectGraph`.
    /// Call edges must have been extracted before calling this.
    pub fn build(graph: &ProjectGraph) -> Self {
        if graph.call_edges.is_empty() {
            return FlowPathIndex::default();
        }

        // Build: callee_name → number of distinct callers
        let caller_counts = build_caller_counts(graph);

        // Find entry points
        let entries = find_entry_points(graph, &caller_counts);

        let mut paths: Vec<FlowPath> = Vec::new();

        for entry_key in &entries {
            let steps = trace_bfs(entry_key, graph, &caller_counts);
            if steps.len() < MIN_PATH_STEPS {
                continue;
            }
            let keywords = extract_keywords(entry_key, &steps);
            let name = path_name(entry_key, &steps);
            let id = path_id(entry_key, &steps);
            paths.push(FlowPath { id, name, entry_key: entry_key.clone(), steps, keywords });
        }

        dedup_paths(&mut paths);

        FlowPathIndex {
            paths,
            built_at: chrono::Utc::now().timestamp(),
        }
    }

    /// Build and immediately persist to `.parecode/paths.json`.
    pub fn build_and_save(graph: &ProjectGraph, root: &Path) -> Self {
        let index = Self::build(graph);
        index.save(root);
        index
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    pub fn save(&self, root: &Path) {
        let dir = root.join(".parecode");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(root.join(PATHS_FILE), json);
        }
    }

    pub fn load(root: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(root.join(PATHS_FILE)).ok()?;
        serde_json::from_str(&content).ok()
    }

    // ── Matching ──────────────────────────────────────────────────────────────

    /// Match a user task description against the path index.
    ///
    /// Scores each path by keyword overlap with the task, boosted by any
    /// file/symbol names the user explicitly mentioned. Returns a `PathMatch`
    /// indicating whether one path won clearly, multiple paths are close, or
    /// nothing matched.
    pub fn match_task(&self, task: &str, attached_files: &[String]) -> PathMatch {
        if self.paths.is_empty() {
            return PathMatch::None;
        }

        // Tokenise the task
        let mut task_words: HashSet<String> = HashSet::new();
        for word in task.split_whitespace() {
            task_words.extend(split_identifier(word));
        }

        // Boost from attached file names
        for f in attached_files {
            let stem = Path::new(f)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            task_words.extend(split_identifier(&stem));
        }

        task_words.retain(|w| w.len() >= 3);

        if task_words.is_empty() {
            return PathMatch::None;
        }

        // Score each path: overlap / sqrt(path_keyword_count)
        let mut scored: Vec<(usize, f32)> = self
            .paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let kw_set: HashSet<&str> =
                    path.keywords.iter().map(|s| s.as_str()).collect();
                let overlap = task_words
                    .iter()
                    .filter(|w| kw_set.contains(w.as_str()))
                    .count();
                let score = overlap as f32
                    / (path.keywords.len().max(1) as f32).sqrt();
                (i, score)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let positive: Vec<_> = scored.iter().filter(|(_, s)| *s > 0.0).collect();

        if positive.is_empty() {
            return PathMatch::None;
        }

        let top_score = positive[0].1;

        // Clear match: top score at least 2× the second place
        if positive.len() == 1 || top_score >= positive[1].1 * 2.0 {
            return PathMatch::Clear(positive[0].0);
        }

        // Ambiguous: collect candidates within half the top score
        let threshold = top_score / 2.0;
        let candidates: Vec<usize> = positive
            .iter()
            .take(3)
            .filter(|(_, s)| *s >= threshold)
            .map(|(i, _)| *i)
            .collect();

        PathMatch::Ambiguous(candidates)
    }

    // ── Inspection ────────────────────────────────────────────────────────────

    /// Human-readable summary of all paths — useful for debugging.
    pub fn _summary(&self) -> String {
        if self.paths.is_empty() {
            return "No flow paths built.".to_string();
        }
        let mut out = format!("{} flow paths\n\n", self.paths.len());
        for (i, path) in self.paths.iter().enumerate() {
            out.push_str(&format!("{}. {} ({} steps)\n", i + 1, path.name, path.steps.len()));
            out.push_str(&format!("   entry:    {}\n", path.entry_key));
            for (j, step) in path.steps.iter().enumerate().take(6) {
                let sym = step.split("::").last().unwrap_or(step);
                let file = step.split("::").next().unwrap_or(step);
                out.push_str(&format!("   step {:2}: {} ({})\n", j + 1, sym, file));
            }
            if path.steps.len() > 6 {
                out.push_str(&format!("   ... +{} more\n", path.steps.len() - 6));
            }
            out.push_str(&format!("   keywords: {}\n\n", path.keywords.join(", ")));
        }
        out
    }
}

// ── Entry point detection ─────────────────────────────────────────────────────

/// Build a map of `callee_name → number of distinct callers` from call_edges.
fn build_caller_counts(graph: &ProjectGraph) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for edges in graph.call_edges.values() {
        for edge in edges {
            *counts.entry(edge.callee.clone()).or_default() += 1;
        }
    }
    counts
}

/// Find entry point caller keys.
///
/// Primary:   0 incoming call edges, not a test function, is a function kind.
/// Secondary: ≤1 incoming edge AND ≥SECONDARY_ENTRY_MIN_OUT outgoing edges
///            (subsystem heads that are not technically root-level but drive
///            significant flows).
fn find_entry_points(
    graph: &ProjectGraph,
    caller_counts: &HashMap<String, usize>,
) -> Vec<String> {
    use crate::index::SymbolKind;

    let mut entries: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for sym in &graph.symbols {
        // Only trace from function-like symbols
        if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        // Skip test functions
        if sym.name.starts_with("test_") || sym.name.starts_with("bench_") {
            continue;
        }

        let incoming = caller_counts.get(&sym.name).copied().unwrap_or(0);
        let key = format!("{}::{}", sym.file, sym.name);
        let outgoing = graph.call_edges.get(&key).map(|v| v.len()).unwrap_or(0);

        let is_primary = incoming == 0;
        let is_secondary = incoming <= 1 && outgoing >= SECONDARY_ENTRY_MIN_OUT;

        if (is_primary || is_secondary) && !seen.contains(&key) {
            seen.insert(key.clone());
            entries.push(key);
        }
    }

    entries
}

// ── BFS path tracer ───────────────────────────────────────────────────────────

/// Trace a BFS path from `entry_key` through the call graph.
/// Returns the ordered list of steps (entry excluded).
fn trace_bfs(
    entry_key: &str,
    graph: &ProjectGraph,
    caller_counts: &HashMap<String, usize>,
) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(entry_key.to_string());

    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(entry_key.to_string());

    'outer: while let Some(current) = queue.pop_front() {
        let Some(edges) = graph.call_edges.get(&current) else {
            continue;
        };

        for edge in edges {
            if steps.len() >= MAX_PATH_STEPS {
                break 'outer;
            }

            let callee = &edge.callee;

            // Skip high-ambiguity callees (trait method dispatch)
            let defs = graph.by_name.get(callee).map(|v| v.as_slice()).unwrap_or(&[]);
            if defs.len() > MAX_AMBIGUITY {
                continue;
            }

            // Skip utility functions called by many callers
            let incoming = caller_counts.get(callee).copied().unwrap_or(0);
            if incoming >= UTILITY_THRESHOLD {
                continue;
            }

            // Resolve callee to one or more symbol keys
            for file in defs {
                let key = format!("{}::{}", file, callee);
                if visited.contains(&key) {
                    continue;
                }
                visited.insert(key.clone());
                steps.push(key.clone());
                queue.push_back(key);
            }
        }
    }

    steps
}

// ── Deduplication ─────────────────────────────────────────────────────────────

/// Remove paths that share ≥80% of steps with a longer path already kept.
fn dedup_paths(paths: &mut Vec<FlowPath>) {
    // Longest first — we prefer keeping the richer path
    paths.sort_by_key(|p| std::cmp::Reverse(p.steps.len()));

    let mut keep = vec![true; paths.len()];

    for i in 0..paths.len() {
        if !keep[i] {
            continue;
        }
        let i_steps: HashSet<&str> = paths[i].steps.iter().map(|s| s.as_str()).collect();

        for j in (i + 1)..paths.len() {
            if !keep[j] {
                continue;
            }
            let j_steps: HashSet<&str> = paths[j].steps.iter().map(|s| s.as_str()).collect();
            let overlap = i_steps.intersection(&j_steps).count();
            let min_len = i_steps.len().min(j_steps.len());
            if min_len > 0 && overlap as f32 / min_len as f32 >= DEDUP_OVERLAP {
                keep[j] = false;
            }
        }
    }

    let mut idx = 0;
    paths.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

// ── Keyword extraction ────────────────────────────────────────────────────────

/// Words that appear in nearly every symbol name — not useful for matching.
const COMMON_WORDS: &[&str] = &[
    "get", "set", "new", "run", "load", "save", "from", "into", "with",
    "make", "init", "the", "and", "for", "pub", "mut", "str", "use",
    "add", "all", "has", "not", "try", "via", "out", "map", "fmt",
];

/// Extract domain keywords from all symbol names in a path.
fn extract_keywords(entry_key: &str, steps: &[String]) -> Vec<String> {
    let mut words: HashSet<String> = HashSet::new();

    let all_keys = std::iter::once(entry_key).chain(steps.iter().map(|s| s.as_str()));
    for key in all_keys {
        // Take just the symbol name after the last "::"
        let name = key.split("::").last().unwrap_or(key);
        for word in split_identifier(name) {
            if word.len() >= 3 && !COMMON_WORDS.contains(&word.as_str()) {
                words.insert(word);
            }
        }
    }

    let mut kw: Vec<String> = words.into_iter().collect();
    kw.sort();
    kw
}

/// Split a snake_case or camelCase identifier into lowercase words.
///
/// `run_tui`        → ["run", "tui"]
/// `dispatch_tool`  → ["dispatch", "tool"]
/// `AppState`       → ["app", "state"]
/// `PIEv2`          → ["pi", "ev2"]  (best-effort; acronyms aren't split)
pub fn split_identifier(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();

    for ch in name.chars() {
        if ch == '_' || ch == '-' || ch == '.' {
            if !current.is_empty() {
                words.push(current.to_lowercase());
                current = String::new();
            }
        } else if ch.is_uppercase() && !current.is_empty() {
            words.push(current.to_lowercase());
            current = ch.to_string();
        } else if ch.is_alphanumeric() {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words
}

// ── Path naming ───────────────────────────────────────────────────────────────

fn sym_name(key: &str) -> &str {
    key.split("::").last().unwrap_or(key)
}

fn path_name(entry_key: &str, steps: &[String]) -> String {
    let entry = sym_name(entry_key);
    match steps.last() {
        Some(last) => format!("{} → {}", entry, sym_name(last)),
        None => entry.to_string(),
    }
}

fn path_id(entry_key: &str, steps: &[String]) -> String {
    let entry = sym_name(entry_key).replace('_', "-");
    let terminal = steps
        .last()
        .map(|s| sym_name(s).replace('_', "-"))
        .unwrap_or_else(|| "end".to_string());
    format!("{}-to-{}", entry, terminal)
}

// ── Phase 3 context delivery ──────────────────────────────────────────────────

/// Pre-load actual source code for a matched flow path, scoped by depth to
/// manage tokens:
///
/// - Depth 0 (entry):         full body (always read from disk)
/// - Depths 1–2 (near steps): body if ≤ 30 lines, else signature only
/// - Depths 3+ (far steps):   signature only (zero disk reads)
///
/// Returns a formatted Markdown block ready for injection before the user task.
pub fn build_path_context(
    path: &FlowPath,
    graph: &crate::pie::ProjectGraph,
    root: &Path,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Relevant call chain: {}\n\n", path.name));

    for (depth, key) in path.all_keys().enumerate() {
        // Find the symbol — key format is "file::symbol_name"
        let Some(sym) = graph.symbols.iter().find(|s| {
            format!("{}::{}", s.file, s.name) == key
        }) else {
            continue;
        };

        let body_lines = sym.end_line.saturating_sub(sym.line) + 1;
        let include_body = depth == 0 || (depth <= 2 && body_lines <= 30);

        out.push_str(&format!("## `{}` ({}:{})\n", sym.name, sym.file, sym.line));

        if include_body {
            let file_path = root.join(&sym.file);
            match std::fs::read_to_string(&file_path) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = sym.line.saturating_sub(1); // convert 1-indexed to 0-indexed
                    let end = sym.end_line.min(lines.len());
                    if start < end {
                        out.push_str("```rust\n");
                        out.push_str(&lines[start..end].join("\n"));
                        out.push_str("\n```\n");
                    } else {
                        append_sig(&mut out, sym);
                    }
                }
                Err(_) => append_sig(&mut out, sym),
            }
        } else {
            append_sig(&mut out, sym);
        }

        out.push('\n');
    }

    out
}

fn append_sig(out: &mut String, sym: &crate::index::Symbol) {
    if let Some(sig) = &sym.signature {
        out.push_str(&format!("```rust\n{} {}{} {{ … }}\n```\n", sym.kind.label(), sym.name, sig));
    } else {
        out.push_str(&format!("*{} `{}` — signature not indexed*\n", sym.kind.label(), sym.name));
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::index::{CallEdge, Symbol, SymbolKind};
    use crate::pie::{Cluster, ProjectGraph};

    fn fn_sym(name: &str, file: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            file: file.to_string(),
            line,
            end_line: line + 10,
            kind: SymbolKind::Function,
            signature: None,
        }
    }

    fn make_graph(
        symbols: Vec<Symbol>,
        call_edges: HashMap<String, Vec<CallEdge>>,
    ) -> ProjectGraph {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for s in &symbols {
            by_name.entry(s.name.clone()).or_default().push(s.file.clone());
        }
        let file_lines: HashMap<String, usize> = symbols
            .iter()
            .map(|s| (s.file.clone(), 100usize))
            .collect();
        ProjectGraph {
            schema_version: 2,
            file_hashes: HashMap::new(),
            file_lines,
            symbols,
            by_name,
            clusters: Vec::new(),
            last_indexed: 0,
            call_edges,
            construct_edges: HashMap::new(),
        }
    }

    #[test]
    fn test_split_identifier() {
        assert_eq!(split_identifier("run_tui"), vec!["run", "tui"]);
        assert_eq!(split_identifier("dispatch_tool"), vec!["dispatch", "tool"]);
        assert_eq!(split_identifier("AppState"), vec!["app", "state"]);
        assert_eq!(split_identifier("generatePlan"), vec!["generate", "plan"]);
        assert_eq!(split_identifier("foo"), vec!["foo"]);
    }

    #[test]
    fn test_trace_simple_chain() {
        // main -> run_tui -> dispatch_tool
        let symbols = vec![
            fn_sym("main", "src/main.rs", 1),
            fn_sym("run_tui", "src/agent.rs", 1),
            fn_sym("dispatch_tool", "src/agent.rs", 20),
        ];
        let mut call_edges = HashMap::new();
        call_edges.insert(
            "src/main.rs::main".to_string(),
            vec![CallEdge { callee: "run_tui".to_string(), call_line: 5 }],
        );
        call_edges.insert(
            "src/agent.rs::run_tui".to_string(),
            vec![CallEdge { callee: "dispatch_tool".to_string(), call_line: 10 }],
        );
        let graph = make_graph(symbols, call_edges);
        let index = FlowPathIndex::build(&graph);

        assert!(!index.paths.is_empty(), "expected at least one path");
        let path = &index.paths[0];
        assert_eq!(path.entry_key, "src/main.rs::main");
        assert!(
            path.steps.contains(&"src/agent.rs::run_tui".to_string()),
            "path should include run_tui"
        );
        assert!(
            path.steps.contains(&"src/agent.rs::dispatch_tool".to_string()),
            "path should include dispatch_tool"
        );
    }

    #[test]
    fn test_test_functions_excluded() {
        let symbols = vec![
            fn_sym("test_something", "src/agent.rs", 1),
            fn_sym("real_fn", "src/agent.rs", 20),
        ];
        let mut call_edges = HashMap::new();
        call_edges.insert(
            "src/agent.rs::test_something".to_string(),
            vec![CallEdge { callee: "real_fn".to_string(), call_line: 5 }],
        );
        let graph = make_graph(symbols, call_edges);
        let index = FlowPathIndex::build(&graph);

        // test_something should not appear as an entry point
        for path in &index.paths {
            assert_ne!(
                path.entry_key, "src/agent.rs::test_something",
                "test functions should not be entry points"
            );
        }
    }

    #[test]
    fn test_utility_skipped() {
        // helper is called by 8 different callers — should be treated as utility
        let symbols = vec![
            fn_sym("entry", "src/a.rs", 1),
            fn_sym("helper", "src/b.rs", 1),
            fn_sym("caller2", "src/c.rs", 1),
            fn_sym("caller3", "src/d.rs", 1),
            fn_sym("caller4", "src/e.rs", 1),
            fn_sym("caller5", "src/f.rs", 1),
            fn_sym("caller6", "src/g.rs", 1),
            fn_sym("caller7", "src/h.rs", 1),
        ];

        let mut call_edges = HashMap::new();
        call_edges.insert(
            "src/a.rs::entry".to_string(),
            vec![CallEdge { callee: "helper".to_string(), call_line: 5 }],
        );
        // Make helper have 6+ callers so it hits UTILITY_THRESHOLD
        for (i, caller) in ["caller2","caller3","caller4","caller5","caller6","caller7"].iter().enumerate() {
            call_edges.insert(
                format!("src/{}::{}", (b'c' + i as u8) as char, caller),
                vec![CallEdge { callee: "helper".to_string(), call_line: 5 }],
            );
        }

        let graph = make_graph(symbols, call_edges);
        let index = FlowPathIndex::build(&graph);

        // If a path from "entry" was traced, "helper" should not appear in its steps
        for path in &index.paths {
            if path.entry_key == "src/a.rs::entry" {
                assert!(
                    !path.steps.iter().any(|s| s.contains("helper")),
                    "utility function should not appear in path steps"
                );
            }
        }
    }

    #[test]
    fn test_cycle_prevention() {
        // a -> b -> a (cycle)
        let symbols = vec![
            fn_sym("a", "src/a.rs", 1),
            fn_sym("b", "src/b.rs", 1),
        ];
        let mut call_edges = HashMap::new();
        call_edges.insert(
            "src/a.rs::a".to_string(),
            vec![CallEdge { callee: "b".to_string(), call_line: 2 }],
        );
        call_edges.insert(
            "src/b.rs::b".to_string(),
            vec![CallEdge { callee: "a".to_string(), call_line: 2 }],
        );
        let graph = make_graph(symbols, call_edges);
        // Should not hang or stack overflow
        let index = FlowPathIndex::build(&graph);
        // Just verify it completes and produces something sensible
        for path in &index.paths {
            assert!(path.steps.len() <= MAX_PATH_STEPS);
        }
    }

    #[test]
    fn test_match_task_clear() {
        let path = FlowPath {
            id: "plan-test".to_string(),
            name: "generate_plan → execute_step".to_string(),
            entry_key: "src/plan.rs::generate_plan".to_string(),
            steps: vec!["src/plan.rs::execute_step".to_string()],
            keywords: vec!["execute".to_string(), "generate".to_string(), "plan".to_string(), "step".to_string()],
        };
        let index = FlowPathIndex { paths: vec![path], built_at: 0 };

        // "plan mode is broken" should clearly match
        let result = index.match_task("plan mode is broken", &[]);
        assert!(matches!(result, PathMatch::Clear(_)));
    }

    #[test]
    fn test_match_task_no_match() {
        let path = FlowPath {
            id: "plan-test".to_string(),
            name: "generate_plan → execute_step".to_string(),
            entry_key: "src/plan.rs::generate_plan".to_string(),
            steps: vec![],
            keywords: vec!["generate".to_string(), "plan".to_string()],
        };
        let index = FlowPathIndex { paths: vec![path], built_at: 0 };

        // Completely unrelated task
        let result = index.match_task("fix the button colour", &[]);
        assert!(matches!(result, PathMatch::None));
    }
}
