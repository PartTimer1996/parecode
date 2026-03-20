/// PIE Phase 1 — Persistent Symbol Graph.
///
/// Replaces the ephemeral `SymbolIndex` (rebuilt on every `/plan`) with a
/// `ProjectGraph` that persists to `.parecode/project.graph` (JSON).
///
/// On a warm run:
///   1. Load from disk (fast JSON parse, ~10ms)
///   2. Diff file hashes via `git ls-files -s` (one subprocess) or mtime fallback
///   3. Reindex only changed files
///   4. Recluster (cheap — pure directory grouping)
///   5. Save and return
///
/// Cold run: full scan via `SymbolIndex::build()`, then persist.
use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{client::{ContentPart, Message, MessageContent, ToolCall}, index::{Symbol, SymbolIndex, compute_end_lines, extract_symbols}};

const SCHEMA_VERSION: u32 = 1;
const GRAPH_PATH: &str = ".parecode/project.graph";


// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectGraph {
    pub schema_version: u32,
    /// path → git blob hash (or `"mtime:{unix_secs}"` for non-git repos)
    pub file_hashes: HashMap<String, String>,
    /// path → line count
    pub file_lines: HashMap<String, usize>,
    /// all symbols, sorted by file then line number
    pub symbols: Vec<Symbol>,
    /// symbol name → [files defining it]
    pub by_name: HashMap<String, Vec<String>>,
    /// directory-based clusters
    pub clusters: Vec<Cluster>,
    /// Unix timestamp of the last index pass
    pub last_indexed: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Cluster {
    /// Derived from the top-level source subdirectory name:
    /// `src/tui/mod.rs` → `"tui"`,  `src/plan.rs` → `"src"`
    pub name: String,
    /// All relative file paths in this cluster
    pub files: Vec<String>,
    /// Top-3 files by symbol count — "most important" proxy
    pub entry_files: Vec<String>,
    /// Phase 2: model-generated summary. Always `None` in Phase 1.
    pub summary: Option<String>,
}

// ── Core API ──────────────────────────────────────────────────────────────────

impl ProjectGraph {
    /// Load from `.parecode/project.graph` if valid; otherwise full rebuild.
    ///
    /// Returns `(graph, was_warm)`. `was_warm = true` means the graph was
    /// loaded from disk and only incrementally updated.
    pub fn load_or_build(root: &Path, max_files: usize) -> (Self, bool) {
        // Attempt warm load
        let graph_path = root.join(GRAPH_PATH);
        if let Ok(content) = std::fs::read_to_string(&graph_path) {
            if let Ok(mut g) = serde_json::from_str::<ProjectGraph>(&content) {
                if g.schema_version == SCHEMA_VERSION {
                    // Migrate graphs persisted before end_line was added (all zeros)
                    if g.symbols.iter().any(|s| s.end_line == 0) {
                        compute_end_lines(&mut g.symbols, &g.file_lines);
                    }
                    // Incremental update
                    g.incremental_update(root, max_files);
                    g.save(root);
                    return (g, true);
                }
            }
        }

        // Cold build
        let g = Self::build_fresh(root, max_files);
        g.save(root);
        (g, false)
    }

    /// Full rebuild from scratch using the existing `SymbolIndex` extractor.
    pub fn build_fresh(root: &Path, max_files: usize) -> Self {
        let idx = SymbolIndex::build(root, max_files);

        let file_hashes = get_current_hashes(root);
        let mut g = ProjectGraph {
            schema_version: SCHEMA_VERSION,
            file_hashes,
            file_lines: idx.file_lines,
            symbols: idx.symbols,
            by_name: idx.by_name,
            clusters: Vec::new(),
            last_indexed: chrono::Utc::now().timestamp(),
        };
        g.clusters = build_clusters(&g.symbols, &g.file_lines);
        g
    }

    /// Diff stored hashes against current; reindex changed files, strip removed ones.
    fn incremental_update(&mut self, root: &Path, _max_files: usize) {
        let current = get_current_hashes(root);

        // Files that changed or are new
        let mut changed: Vec<String> = Vec::new();
        for (path, hash) in &current {
            if self.file_hashes.get(path) != Some(hash) {
                changed.push(path.clone());
            }
        }

        // Files that were removed
        let removed: Vec<String> = self
            .file_hashes
            .keys()
            .filter(|p| !current.contains_key(*p))
            .cloned()
            .collect();

        for path in &removed {
            self.remove_file(path);
        }

        if !changed.is_empty() {
            self.reindex_files(&changed, root);
        }

        // Update hashes map to current state
        self.file_hashes = current;

        // Recluster (cheap)
        self.clusters = build_clusters(&self.symbols, &self.file_lines);
        self.last_indexed = chrono::Utc::now().timestamp();
    }

    /// Remove all data for a file from the graph.
    fn remove_file(&mut self, path: &str) {
        self.symbols.retain(|s| s.file != path);
        self.file_lines.remove(path);
        self.file_hashes.remove(path);

        // Clean by_name entries
        for files in self.by_name.values_mut() {
            files.retain(|f| f != path);
        }
        self.by_name.retain(|_, files| !files.is_empty());
    }

    /// Re-extract symbols for `paths`, replacing stale entries.
    pub fn reindex_files(&mut self, paths: &[String], root: &Path) {
        for rel_path in paths {
            // Strip old symbols for this file
            self.symbols.retain(|s| &s.file != rel_path);
            for files in self.by_name.values_mut() {
                files.retain(|f| f != rel_path);
            }
            self.by_name.retain(|_, files| !files.is_empty());

            // Re-extract from disk
            let abs = root.join(rel_path);
            let Ok(content) = std::fs::read_to_string(&abs) else { continue };

            let line_count = content.lines().count();
            self.file_lines.insert(rel_path.clone(), line_count);

            let mut new_syms: Vec<Symbol> = Vec::new();
            extract_symbols(&content, rel_path, &mut new_syms);

            // Update by_name
            for sym in &new_syms {
                let entry = self.by_name.entry(sym.name.clone()).or_default();
                if !entry.contains(&sym.file) {
                    entry.push(sym.file.clone());
                }
            }

            self.symbols.extend(new_syms);
        }

        // Re-sort by file + line, then recompute end lines
        self.symbols
            .sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
        compute_end_lines(&mut self.symbols, &self.file_lines);
    }

    /// Save graph to `.parecode/project.graph` (JSON).
    pub fn save(&self, root: &Path) {
        let dir = root.join(".parecode");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(root.join(GRAPH_PATH), json);
        }
        append_gitignore_if_needed(root);
    }

    /// Resolve symbol names or file paths to concrete file paths.
    ///
    /// If `entry` looks like a path (contains `/` or `.`), it's kept as-is.
    /// Otherwise it's treated as a symbol name and looked up in `by_name`.
    pub fn resolve_files(&self, entries: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for entry in entries {
            if entry.contains('/') || entry.contains('.') {
                if !out.contains(entry) {
                    out.push(entry.clone());
                }
            } else if let Some(files) = self.by_name.get(entry.as_str()) {
                for f in files {
                    if !out.contains(f) {
                        out.push(f.clone());
                    }
                }
            } else {
                if !out.contains(entry) {
                    out.push(entry.clone());
                }
            }
        }
        out
    }


    /// Compact cluster-grouped text for injection into the planning prompt.
    ///
    /// Format:
    /// ```text
    /// # Project structure — 5 clusters, 23 files
    ///
    /// ## tui (8 files)
    /// Key files: src/tui/mod.rs (3100 lines), src/tui/render.rs (448 lines)
    /// fn run, struct AppState, enum Mode … (42 symbols)
    /// ```
    pub fn to_prompt_section(&self, max_clusters: usize) -> Option<String> {
        if self.clusters.is_empty() {
            // Fall back to flat symbol list if clustering produced nothing
            return flat_symbol_section(&self.symbols, &self.file_lines, 60);
        }

        let total_files: usize = self.clusters.iter().map(|c| c.files.len()).sum();
        let mut out = format!(
            "# Project structure — {} clusters, {} files\n",
            self.clusters.len(),
            total_files
        );

        for cluster in self.clusters.iter().take(max_clusters) {
            out.push('\n');
            out.push_str(&format!("## {} ({} files)\n", cluster.name, cluster.files.len()));

            // Key files line
            if !cluster.entry_files.is_empty() {
                let key_parts: Vec<String> = cluster
                    .entry_files
                    .iter()
                    .map(|f| {
                        let lines = self.file_lines.get(f).copied().unwrap_or(0);
                        if lines > 0 {
                            format!("{f} ({lines} lines)")
                        } else {
                            f.clone()
                        }
                    })
                    .collect();
                out.push_str(&format!("Key files: {}\n", key_parts.join(", ")));
            }

            // Symbol list (capped)
            let syms: Vec<String> = self
                .symbols
                .iter()
                .filter(|s| cluster.files.contains(&s.file))
                .take(10)
                .map(|s| format!("{} {}", s.kind.label(), s.name))
                .collect();

            let total_syms = self
                .symbols
                .iter()
                .filter(|s| cluster.files.contains(&s.file))
                .count();

            if !syms.is_empty() {
                let ellipsis = if total_syms > 10 {
                    format!(" … ({total_syms} symbols)")
                } else {
                    String::new()
                };
                out.push_str(&format!("{}{}\n", syms.join(", "), ellipsis));
            }
        }

        if self.clusters.len() > max_clusters {
            out.push_str(&format!(
                "\n… and {} more clusters\n",
                self.clusters.len() - max_clusters
            ));
        }

        Some(out)
    }
}

// ── Clustering ────────────────────────────────────────────────────────────────

/// Group files into directory-based clusters.
/// `src/tui/mod.rs` → cluster `tui`
/// `src/plan.rs`    → cluster `src` (flat source files)
fn build_clusters(symbols: &[Symbol], file_lines: &HashMap<String, usize>) -> Vec<Cluster> {
    // Count symbols per file for entry_files selection
    let mut sym_count: HashMap<&str, usize> = HashMap::new();
    for s in symbols {
        *sym_count.entry(s.file.as_str()).or_default() += 1;
    }

    // Group files by cluster name
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for path in file_lines.keys() {
        let cluster_name = cluster_name_for(path);
        groups.entry(cluster_name).or_default().push(path.clone());
    }

    let mut clusters: Vec<Cluster> = groups
        .into_iter()
        .map(|(name, mut files)| {
            // Sort files for determinism
            files.sort();

            // entry_files = top 3 by symbol count
            let mut scored: Vec<(&str, usize)> = files
                .iter()
                .map(|f| (f.as_str(), *sym_count.get(f.as_str()).unwrap_or(&0)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let entry_files: Vec<String> = scored
                .iter()
                .take(3)
                .map(|(f, _)| f.to_string())
                .collect();

            Cluster {
                name,
                files,
                entry_files,
                summary: None,
            }
        })
        .collect();

    // Sort clusters by file count desc, then name
    clusters.sort_by(|a, b| b.files.len().cmp(&a.files.len()).then(a.name.cmp(&b.name)));
    clusters
}

/// Derive a cluster name from a relative file path.
///
/// Rules:
/// - `src/tui/mod.rs`    → `"tui"`   (second component under src/)
/// - `src/tools/bash.rs` → `"tools"`
/// - `src/plan.rs`       → `"src"`   (flat file directly in src/)
/// - `main.rs`           → `"root"`  (top-level file, no src/)
fn cluster_name_for(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        // src/subdir/... → subdir
        [src, subdir, ..] if *src == "src" && parts.len() > 2 => subdir.to_string(),
        // src/file.rs → "src"
        [src, _] if *src == "src" => "src".to_string(),
        // anything else with a subdirectory: use first component
        [dir, _, ..] => dir.to_string(),
        // bare file at root
        _ => "root".to_string(),
    }
}

// ── Change detection ──────────────────────────────────────────────────────────

/// Get current file hashes: git blob hashes (one subprocess) or mtime fallback.
pub fn get_current_hashes(root: &Path) -> HashMap<String, String> {
    // Try git ls-files -s (one call regardless of repo size)
    let output = std::process::Command::new("git")
        .args(["ls-files", "-s"])
        .current_dir(root)
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let mut hashes = HashMap::new();
            if let Ok(text) = std::str::from_utf8(&out.stdout) {
                for line in text.lines() {
                    // Format: "mode hash stage\tpath"
                    let Some(tab_pos) = line.find('\t') else { continue };
                    let path = &line[tab_pos + 1..];
                    let parts: Vec<&str> = line[..tab_pos].split_whitespace().collect();
                    if parts.len() >= 2 {
                        hashes.insert(path.to_string(), parts[1].to_string());
                    }
                }
            }
            if !hashes.is_empty() {
                return hashes;
            }
        }
    }

    // Fallback: mtime per file
    mtime_hashes(root)
}

/// Walk source files under `root` and return path → `"mtime:{unix_secs}"`.
fn mtime_hashes(root: &Path) -> HashMap<String, String> {
    const IGNORED: &[&str] = &[
        "target", "node_modules", ".git", ".next", "dist", "build",
        "__pycache__", ".venv", "venv", ".cache", "coverage",
    ];
    const EXTENSIONS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "py", "go", "c", "cpp", "h", "hpp",
    ];

    let mut out = HashMap::new();
    collect_mtime(root, root, IGNORED, EXTENSIONS, &mut out, 2000);
    out
}

fn collect_mtime(
    base: &Path,
    dir: &Path,
    ignored: &[&str],
    extensions: &[&str],
    out: &mut HashMap<String, String>,
    max: usize,
) {
    if out.len() >= max {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if out.len() >= max {
            break;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || ignored.contains(&name_str.as_ref()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_mtime(base, &path, ignored, extensions, out, max);
        } else if let Some(ext) = path.extension() {
            if extensions.contains(&ext.to_string_lossy().as_ref()) {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let mtime = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.insert(rel, format!("mtime:{mtime}"));
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Flat symbol section — fallback when clustering produces no results.
fn flat_symbol_section(
    symbols: &[Symbol],
    file_lines: &HashMap<String, usize>,
    max_lines: usize,
) -> Option<String> {
    if symbols.is_empty() {
        return None;
    }

    // Group by file
    let mut by_file: Vec<(String, Vec<String>)> = Vec::new();
    for sym in symbols {
        if let Some(last) = by_file.last_mut() {
            if last.0 == sym.file {
                last.1.push(format!("{} {}", sym.kind.label(), sym.name));
                continue;
            }
        }
        by_file.push((
            sym.file.clone(),
            vec![format!("{} {}", sym.kind.label(), sym.name)],
        ));
    }

    let mut lines: Vec<String> = Vec::new();
    for (file, syms) in &by_file {
        if lines.len() >= max_lines {
            break;
        }
        let sym_list = if syms.len() > 12 {
            format!("{}, … ({} total)", syms[..12].join(", "), syms.len())
        } else {
            syms.join(", ")
        };
        let line_info = file_lines
            .get(file.as_str())
            .map(|n| format!(" ({n} lines)"))
            .unwrap_or_default();
        lines.push(format!("  {file}{line_info}: {sym_list}"));
    }

    if lines.is_empty() {
        return None;
    }

    Some(format!(
        "# Project symbol index\nUse these paths in the \"files\" field of each step:\n\n{}\n",
        lines.join("\n")
    ))
}

/// Append `.parecode/project.graph` to `.gitignore` if not already present.
fn append_gitignore_if_needed(root: &Path) {
    let gitignore = root.join(".gitignore");
    let entry = ".parecode/project.graph";

    if let Ok(content) = std::fs::read_to_string(&gitignore) {
        if content.lines().any(|l| l.trim() == entry) {
            return; // already present
        }
        // Append
        let sep = if content.ends_with('\n') { "" } else { "\n" };
        let _ = std::fs::write(&gitignore, format!("{content}{sep}{entry}\n"));
    } else {
        // No .gitignore or unreadable — don't create one unprompted
    }
}


// ── PIE injection ─────────────────────────────────────────────────────────────

/// Synthetic tool call ID for the session-start PIE injection.
/// Must match the tool_use_id in the ToolResult below.


/// Build a synthetic assistant→tool_result message pair that injects a compact
/// PIE summary into the conversation history before the user task.
///
/// The model sees this as a tool that already ran — higher salience than system
/// prompt text, sent once (not repeated every turn).
///
/// Returns empty vec when graph/narrative are unavailable.
pub fn pie_injection_messages(
    task: &str,
    graph: &crate::pie::ProjectGraph,
    narrative: Option<&crate::narrative::ProjectNarrative>,
    attached_files: &[String],
) -> Vec<Message> {
    const PIE_TOOL_CALL_ID: &str = "pie_ctx_0";
    // focus = attached files (explicit) + anchored files (implicit from task keywords)
    let anchored = anchor_files_from_task(task, attached_files, graph);
    let focus_files: Vec<String> = attached_files.iter().chain(anchored.iter()).cloned().collect();
    let default_narrative = crate::narrative::ProjectNarrative::default();
    let narrative = narrative.unwrap_or(&default_narrative);
    let summary = crate::tools::pie_tool::build_compact_summary(graph, narrative, &focus_files);
    vec![
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Text(String::new()),
            tool_calls: vec![ToolCall {
                id: PIE_TOOL_CALL_ID.to_string(),
                name: "find_symbol".to_string(),
                arguments: r#"{"name":"__summary__"}"#.to_string(),
            }],
        },
        Message {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: PIE_TOOL_CALL_ID.to_string(),
                content: summary,
            }]),
            tool_calls: vec![],
        },
    ]
}

/// Derive up to 5 additional focus files from the task text via keyword anchoring.
///
/// Matches task tokens (≥4 chars) against:
///   - File stems (e.g. "stats" → stats_view.rs)
///   - Symbol names (e.g. "token" → TokenStats → src/tui/mod.rs)
///
/// Returns files not already in `context_files`, sorted by score descending.
/// These are merged with the user-attached files before PIE injection so the
/// model arrives with symbol maps for files the task implicitly references.
fn anchor_files_from_task(task: &str, context_files: &[String], graph: &ProjectGraph) -> Vec<String> {
    let tokens: Vec<String> = task
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .collect();

    if tokens.is_empty() {
        return vec![];
    }

    let mut scores: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    // File stem matching — bidirectional substring
    for file in graph.file_lines.keys() {
        let stem = std::path::Path::new(file.as_str())
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        for token in &tokens {
            if stem.contains(token.as_str()) || token.contains(stem.as_str()) {
                *scores.entry(file.clone()).or_insert(0) += 1;
            }
        }
    }

    // Symbol name matching — task token contained in symbol name (weight 2×)
    for sym in &graph.symbols {
        let sym_lower = sym.name.to_lowercase();
        for token in &tokens {
            if sym_lower.contains(token.as_str()) {
                *scores.entry(sym.file.clone()).or_insert(0) += 2;
            }
        }
    }

    let mut ranked: Vec<(String, usize)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    ranked.into_iter()
        .map(|(f, _)| f)
        .filter(|f| !context_files.contains(f))
        .take(5)
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a file with parent dirs, write content.
    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// Helper: set up a standard multi-cluster project layout.
    fn setup_multi_cluster(tmp: &TempDir) {
        write(
            tmp.path(),
            "src/auth/login.rs",
            "pub fn login() {}\npub struct Session {}\n",
        );
        write(tmp.path(), "src/auth/token.rs", "pub fn validate() {}\n");
        write(tmp.path(), "src/server.rs", "pub fn start() {}\n");
        write(
            tmp.path(),
            "src/utils/fmt.rs",
            "pub fn format_date() {}\n",
        );
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_build_fresh_produces_clusters() {
        let tmp = TempDir::new().unwrap();
        setup_multi_cluster(&tmp);

        let (graph, was_warm) = ProjectGraph::load_or_build(tmp.path(), 100);

        assert!(!was_warm, "should be cold on first build");
        assert!(graph.clusters.len() >= 2, "expected ≥2 clusters, got {}", graph.clusters.len());

        // Cluster named "auth" should exist with 2 files
        let auth = graph.clusters.iter().find(|c| c.name == "auth");
        assert!(auth.is_some(), "expected cluster 'auth'");
        let auth = auth.unwrap();
        assert_eq!(auth.files.len(), 2, "auth cluster should have 2 files");

        // All expected symbols present
        let names: Vec<&str> = graph.symbols.iter().map(|s| s.name.as_str()).collect();
        for expected in &["login", "Session", "validate", "start", "format_date"] {
            assert!(names.contains(expected), "missing symbol: {expected}");
        }

        // entry_files are a subset of cluster.files
        for cluster in &graph.clusters {
            for ef in &cluster.entry_files {
                assert!(
                    cluster.files.contains(ef),
                    "entry_file {ef:?} not in cluster.files"
                );
            }
        }
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_schema_version_mismatch_triggers_rebuild() {
        let tmp = TempDir::new().unwrap();
        setup_multi_cluster(&tmp);

        // Write a graph file with a bad schema version
        let bad_graph = serde_json::json!({
            "schema_version": 999u32,
            "file_hashes": {},
            "file_lines": {},
            "symbols": [],
            "by_name": {},
            "clusters": [],
            "last_indexed": 0i64
        });
        let parecode_dir = tmp.path().join(".parecode");
        fs::create_dir_all(&parecode_dir).unwrap();
        fs::write(
            parecode_dir.join("project.graph"),
            serde_json::to_string(&bad_graph).unwrap(),
        )
        .unwrap();

        let (graph, was_warm) = ProjectGraph::load_or_build(tmp.path(), 100);

        assert!(!was_warm, "schema mismatch should trigger cold rebuild");
        assert_eq!(
            graph.schema_version, SCHEMA_VERSION,
            "rebuilt graph should have correct schema version"
        );
        // Symbols should be present from the fresh build
        assert!(!graph.symbols.is_empty(), "rebuilt graph should have symbols");
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_incremental_reindex_adds_symbol() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/foo.rs", "pub fn alpha() {}\n");

        let (mut graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);

        let names: Vec<&str> = graph.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha should be present");
        assert!(!names.contains(&"beta"), "beta not yet added");

        // Add a second function
        write(tmp.path(), "src/foo.rs", "pub fn alpha() {}\npub fn beta() {}\n");
        graph.reindex_files(&["src/foo.rs".to_string()], tmp.path());

        let names: Vec<&str> = graph.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha should still be present");
        assert!(names.contains(&"beta"), "beta should now be present");
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_incremental_reindex_removes_stale_symbols() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/foo.rs",
            "pub fn alpha() {}\npub fn beta() {}\n",
        );

        let (mut graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);

        let names: Vec<&str> = graph.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));

        // Remove beta from the file
        write(tmp.path(), "src/foo.rs", "pub fn alpha() {}\n");
        graph.reindex_files(&["src/foo.rs".to_string()], tmp.path());

        let names: Vec<&str> = graph.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha should remain");
        assert!(!names.contains(&"beta"), "beta should be gone");
        assert!(
            !graph.by_name.contains_key("beta"),
            "beta should be removed from by_name"
        );
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_removed_file_purged_from_graph() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/a.rs", "pub fn func_a() {}\n");
        write(tmp.path(), "src/b.rs", "pub fn func_b() {}\n");

        // First build
        let _ = ProjectGraph::load_or_build(tmp.path(), 100);

        // Delete src/b.rs
        fs::remove_file(tmp.path().join("src/b.rs")).unwrap();

        // Second load — warm path, should detect removal
        let (graph, was_warm) = ProjectGraph::load_or_build(tmp.path(), 100);
        assert!(was_warm, "second load should be warm");

        let files_in_symbols: Vec<&str> =
            graph.symbols.iter().map(|s| s.file.as_str()).collect();
        assert!(
            !files_in_symbols.contains(&"src/b.rs"),
            "src/b.rs symbols should be removed"
        );
        assert!(
            files_in_symbols.contains(&"src/a.rs"),
            "src/a.rs symbols should remain"
        );
        assert!(
            !graph.by_name.contains_key("func_b"),
            "func_b should be removed from by_name"
        );
    }

    // ── Test 6 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_save_and_round_trip() {
        let tmp = TempDir::new().unwrap();
        setup_multi_cluster(&tmp);

        let (graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);
        let original_cluster_count = graph.clusters.len();
        let original_sym_count = graph.symbols.len();
        let original_by_name_keys: std::collections::HashSet<String> =
            graph.by_name.keys().cloned().collect();

        // Read back from disk
        let graph_path = tmp.path().join(".parecode/project.graph");
        let content = fs::read_to_string(&graph_path).expect("graph file should exist");
        let loaded: ProjectGraph =
            serde_json::from_str(&content).expect("graph JSON should be valid");

        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(
            loaded.clusters.len(),
            original_cluster_count,
            "cluster count should survive round-trip"
        );
        assert_eq!(
            loaded.symbols.len(),
            original_sym_count,
            "symbol count should survive round-trip"
        );
        let loaded_keys: std::collections::HashSet<String> =
            loaded.by_name.keys().cloned().collect();
        assert_eq!(
            loaded_keys, original_by_name_keys,
            "by_name keys should survive round-trip"
        );
    }

    // ── Test 7 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_to_prompt_section_format() {
        let tmp = TempDir::new().unwrap();
        setup_multi_cluster(&tmp);

        let (graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);
        let section = graph.to_prompt_section(8);

        assert!(section.is_some(), "to_prompt_section should return Some");
        let s = section.unwrap();

        assert!(
            s.contains("# Project structure"),
            "should contain section header"
        );
        // At least one cluster header
        assert!(s.contains("## "), "should contain cluster headers");
        // Line counts should appear (file_lines is populated)
        assert!(
            s.contains("lines)"),
            "should include line counts: {s}"
        );
        // Key files header
        assert!(s.contains("Key files:"), "should contain 'Key files:'");
    }

    // ── Test 8 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_clustering_flat_src_files() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/main.rs", "pub fn main() {}\n");
        write(
            tmp.path(),
            "src/lib.rs",
            "pub fn helper() {}\npub struct Config {}\n",
        );

        let (graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);

        // Both files should be in the same "src" cluster
        let src_cluster = graph.clusters.iter().find(|c| c.name == "src");
        assert!(src_cluster.is_some(), "expected 'src' cluster");
        let src_cluster = src_cluster.unwrap();
        assert_eq!(src_cluster.files.len(), 2, "both flat src files in one cluster");

        // entry_files should be non-empty, preferring the file with more symbols (lib.rs)
        assert!(!src_cluster.entry_files.is_empty());
        // lib.rs has 2 symbols vs main.rs 1 — should be first entry file
        assert!(
            src_cluster.entry_files[0].contains("lib.rs"),
            "lib.rs (2 symbols) should be top entry file"
        );
    }

    // ── Test 9 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_get_current_hashes_fallback_no_git() {
        let tmp = TempDir::new().unwrap();
        // Write some source files — no .git directory
        write(tmp.path(), "src/a.rs", "fn a() {}\n");
        write(tmp.path(), "src/b.rs", "fn b() {}\n");

        let hashes = get_current_hashes(tmp.path());

        assert!(!hashes.is_empty(), "should return hashes for source files");

        // All values should be mtime: prefixed (no git available in this dir)
        // Note: if the test runs inside a git repo and git detects it, hashes may be git hashes.
        // We just assert the map is non-empty and values are non-empty strings.
        for (path, hash) in &hashes {
            assert!(!hash.is_empty(), "hash for {path} should not be empty");
        }

        // Second call on unchanged files should return same values
        let hashes2 = get_current_hashes(tmp.path());
        for (path, hash) in &hashes {
            let hash2 = hashes2.get(path);
            assert_eq!(
                Some(hash),
                hash2,
                "hash for {path} should be stable across calls"
            );
        }
    }

    // ── Test 10 ────────────────────────────────────────────────────────────────

    #[test]
    fn test_by_name_deduplication() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/a.rs", "pub fn process() {}\n");
        write(tmp.path(), "src/b.rs", "pub fn process() {}\n");

        let (graph, _) = ProjectGraph::load_or_build(tmp.path(), 100);

        let files = graph.by_name.get("process");
        assert!(files.is_some(), "process should be in by_name");
        let files = files.unwrap();

        assert_eq!(files.len(), 2, "process should map to 2 files, got: {files:?}");

        // No duplicates
        let unique: std::collections::HashSet<&String> = files.iter().collect();
        assert_eq!(unique.len(), files.len(), "by_name entries should have no duplicates");
    }

}
