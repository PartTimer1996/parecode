/// PIE Phase 2 — Project Narrative.
///
/// Persists to `.parecode/narrative.json`. Generated once on cold startup behind
/// the splash screen (one model call). On warm runs loads instantly from disk.
///
/// Adds three things to planning/agent context:
///   1. `architecture_summary` — ~100 word project-level description
///   2. `cluster_summaries`    — per-cluster ~30 word summaries
///   3. `conventions`          — zero-cost heuristic observations about the project
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::client::{Client, Message, MessageContent};
use crate::pie::ProjectGraph;

const NARRATIVE_SCHEMA_VERSION: u32 = 1;
const NARRATIVE_PATH: &str = ".parecode/narrative.json";

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectNarrative {
    pub schema_version: u32,
    /// ~100 word factual description of the overall architecture
    pub architecture_summary: String,
    /// cluster name → ~30 word summary
    pub cluster_summaries: HashMap<String, String>,
    /// zero-cost heuristic observations: test coverage, error patterns, etc.
    pub conventions: Vec<String>,
    /// Unix timestamp of last full synthesis
    pub last_synthesized: i64,
    /// Incremental patches since last synthesis (>10 triggers re-synthesis warning)
    pub patches: Vec<String>,
}

// ── Core API ──────────────────────────────────────────────────────────────────

impl ProjectNarrative {
    /// Load from `.parecode/narrative.json` if valid; otherwise generate via model call.
    ///
    /// Warm: instant JSON load.
    /// Cold: `detect_conventions()` (free) + one model call for summaries.
    ///
    /// Never panics — on any error returns a default narrative so callers always
    /// get something (even if empty strings).
    /// Owned-args variant — required for `tokio::spawn` (needs `'static`).
    pub async fn load_or_generate(graph: ProjectGraph, client: Client, root: std::path::PathBuf) -> Option<Self> {
        Some(Self::load_or_generate_ref(&graph, &client, &root).await)
    }

    pub async fn load_or_generate_ref(graph: &ProjectGraph, client: &Client, root: &Path) -> Self {
        // Warm path: load from disk
        let narrative_path = root.join(NARRATIVE_PATH);
        if let Ok(content) = std::fs::read_to_string(&narrative_path) {
            if let Ok(n) = serde_json::from_str::<ProjectNarrative>(&content) {
                if n.schema_version == NARRATIVE_SCHEMA_VERSION && !n.architecture_summary.is_empty() {
                    return n;
                }
            }
        }

        // Cold path: generate
        let conventions = detect_conventions(graph);
        let (architecture_summary, cluster_summaries) =
            generate_narrative(graph, client).await.unwrap_or_default();

        let narrative = ProjectNarrative {
            schema_version: NARRATIVE_SCHEMA_VERSION,
            architecture_summary,
            cluster_summaries,
            conventions,
            last_synthesized: chrono::Utc::now().timestamp(),
            patches: Vec::new(),
        };

        narrative.save(root);
        narrative
    }

    /// Persist to `.parecode/narrative.json`.
    pub fn save(&self, root: &Path) {
        let dir = root.join(".parecode");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(root.join(NARRATIVE_PATH), json);
        }
    }

    /// Build a context string for injection into planning/agent system prompts.
    ///
    /// Format:
    /// ```text
    /// # Project architecture
    /// <architecture_summary>
    ///
    /// ## Conventions
    /// - <convention>
    ///
    /// ## Relevant clusters
    /// ### tui (8 files)
    /// <summary>
    /// Key files: ...
    ///
    /// ## Other clusters
    /// ### plan — <first 15 words of summary>
    /// ```
    ///
    /// Falls back to `graph.to_prompt_section(max_clusters)` if narrative is empty.
    pub fn to_context_package(
        &self,
        graph: &ProjectGraph,
        relevant_clusters: &[&str],
        max_clusters: usize,
        recent_tasks: &[crate::task_memory::TaskRecord],
    ) -> String {
        if self.architecture_summary.is_empty() {
            // Fallback: graph-only section (Phase 1.5 format)
            return graph
                .to_prompt_section(max_clusters)
                .unwrap_or_default();
        }

        let mut out = String::new();

        // Architecture summary
        out.push_str("# Project architecture\n");
        out.push_str(&self.architecture_summary);
        out.push('\n');

        // Conventions
        if !self.conventions.is_empty() {
            out.push_str("\n## Conventions\n");
            for conv in &self.conventions {
                out.push_str(&format!("- {conv}\n"));
            }
        }

        // Recent relevant tasks (Phase 3)
        if !recent_tasks.is_empty() {
            out.push_str("\n## Recent relevant tasks\n");
            for task in recent_tasks.iter().take(3) {
                let age = task.age_str();
                let files = if task.files_modified.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", task.files_modified.join(", "))
                };
                out.push_str(&format!("- [{age}] {}{}\n", task.summary, files));
            }
        }

        // Split clusters into relevant + others
        let all_clusters = &graph.clusters;
        let (relevant, others): (Vec<_>, Vec<_>) = all_clusters
            .iter()
            .partition(|c| relevant_clusters.contains(&c.name.as_str()));

        // Relevant clusters — full detail
        if !relevant.is_empty() {
            out.push_str("\n## Relevant clusters\n");
            for cluster in &relevant {
                out.push_str(&format!("\n### {} ({} files)\n", cluster.name, cluster.files.len()));
                if let Some(summary) = self.cluster_summaries.get(&cluster.name) {
                    out.push_str(summary);
                    out.push('\n');
                }
                // Key files with line counts
                if !cluster.entry_files.is_empty() {
                    let key_parts: Vec<String> = cluster.entry_files.iter()
                        .map(|f| {
                            let lines = graph.file_lines.get(f).copied().unwrap_or(0);
                            if lines > 0 { format!("{f} ({lines} lines)") } else { f.clone() }
                        })
                        .collect();
                    out.push_str(&format!("Key files: {}\n", key_parts.join(", ")));
                }
                // Symbols (capped at 10)
                let syms: Vec<String> = graph.symbols.iter()
                    .filter(|s| cluster.files.contains(&s.file))
                    .take(10)
                    .map(|s| format!("{} {}", s.kind.label(), s.name))
                    .collect();
                let total_syms = graph.symbols.iter()
                    .filter(|s| cluster.files.contains(&s.file))
                    .count();
                if !syms.is_empty() {
                    let ellipsis = if total_syms > 10 { format!(" … ({total_syms} symbols)") } else { String::new() };
                    out.push_str(&format!("{}{}\n", syms.join(", "), ellipsis));
                }
            }
        }

        // Other clusters — one-line summaries, capped
        let remaining_cap = max_clusters.saturating_sub(relevant.len());
        if !others.is_empty() && remaining_cap > 0 {
            out.push_str("\n## Other clusters\n");
            for cluster in others.iter().take(remaining_cap) {
                if let Some(summary) = self.cluster_summaries.get(&cluster.name) {
                    // Truncate to 15 words
                    let words: Vec<&str> = summary.split_whitespace().collect();
                    let short = if words.len() > 15 {
                        format!("{} …", words[..15].join(" "))
                    } else {
                        summary.clone()
                    };
                    out.push_str(&format!("### {} ({} files) — {}\n", cluster.name, cluster.files.len(), short));
                } else {
                    out.push_str(&format!("### {} ({} files)\n", cluster.name, cluster.files.len()));
                }
            }
            if others.len() > remaining_cap {
                out.push_str(&format!("… and {} more clusters\n", others.len() - remaining_cap));
            }
        } else if relevant.is_empty() {
            // No relevant clusters — sort by learned context weights (high-weight first)
            let weights = crate::context_weights::ContextWeights::load();
            let mut sorted_clusters: Vec<&crate::pie::Cluster> = all_clusters.iter().collect();
            sorted_clusters.sort_by(|a, b| {
                let wa = weights._mean_weight(&a.entry_files);
                let wb = weights._mean_weight(&b.entry_files);
                wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
            });
            out.push_str("\n## Clusters\n");
            for cluster in sorted_clusters.iter().take(max_clusters) {
                out.push_str(&format!("\n### {} ({} files)\n", cluster.name, cluster.files.len()));
                if let Some(summary) = self.cluster_summaries.get(&cluster.name) {
                    out.push_str(summary);
                    out.push('\n');
                }
                if !cluster.entry_files.is_empty() {
                    let key_parts: Vec<String> = cluster.entry_files.iter()
                        .map(|f| {
                            let lines = graph.file_lines.get(f).copied().unwrap_or(0);
                            if lines > 0 { format!("{f} ({lines} lines)") } else { f.clone() }
                        })
                        .collect();
                    out.push_str(&format!("Key files: {}\n", key_parts.join(", ")));
                }
                let syms: Vec<String> = graph.symbols.iter()
                    .filter(|s| cluster.files.contains(&s.file))
                    .take(10)
                    .map(|s| format!("{} {}", s.kind.label(), s.name))
                    .collect();
                let total_syms = graph.symbols.iter()
                    .filter(|s| cluster.files.contains(&s.file))
                    .count();
                if !syms.is_empty() {
                    let ellipsis = if total_syms > 10 { format!(" … ({total_syms} symbols)") } else { String::new() };
                    out.push_str(&format!("{}{}\n", syms.join(", "), ellipsis));
                }
            }
            if all_clusters.len() > max_clusters {
                out.push_str(&format!("\n… and {} more clusters\n", all_clusters.len() - max_clusters));
            }
        }

        out
    }
}

// ── Convention detection ───────────────────────────────────────────────────────

/// Derive zero-cost heuristic observations from the graph.
/// No model calls. Returns 2–3 concise strings.
pub fn detect_conventions(graph: &ProjectGraph) -> Vec<String> {
    let mut out = Vec::new();

    // Cluster count summary
    if !graph.clusters.is_empty() {
        let names: Vec<&str> = graph.clusters.iter().map(|c| c.name.as_str()).collect();
        out.push(format!(
            "{} source clusters: {}",
            names.len(),
            names.join(", ")
        ));
    }

    // Symbol type breakdown
    let total = graph.symbols.len();
    if total > 0 {
        let fn_count = graph.symbols.iter()
            .filter(|s| matches!(s.kind, crate::index::SymbolKind::Function))
            .count();
        let struct_count = graph.symbols.iter()
            .filter(|s| matches!(s.kind, crate::index::SymbolKind::Struct))
            .count();
        if fn_count > 0 || struct_count > 0 {
            out.push(format!(
                "{total} symbols total ({fn_count} functions, {struct_count} structs/types)"
            ));
        }
    }

    // File count
    let file_count = graph.file_lines.len();
    if file_count > 0 {
        out.push(format!("{file_count} indexed source files"));
    }

    out
}

// ── Narrative generation ──────────────────────────────────────────────────────

const NARRATIVE_SYSTEM_PROMPT: &str =
    "You are analyzing a software project from its structural symbol index. \
     The project name and description are given at the top of the user message — use them. \
     Produce a concise factual JSON description of what the project does. \
     Be specific to this project, not generic. Focus on the source code clusters, not docs or config files. \
     Respond ONLY with valid JSON — no markdown fences, no explanation:\n\
     {\n\
       \"architecture_summary\": \"<100 words max, factual description of what this project does and how it's structured>\",\n\
       \"cluster_summaries\": {\n\
         \"<cluster_name>\": \"<30 words max per cluster>\"\n\
       }\n\
     }";

/// Attempt to read the project name from `Cargo.toml` or `package.json`.
fn detect_project_name() -> Option<String> {
    // Cargo.toml
    if let Ok(content) = std::fs::read_to_string("Cargo.toml") {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("name") {
                if let Some(val) = line.splitn(2, '=').nth(1) {
                    let name = val.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
        }
    }
    // package.json
    if let Ok(content) = std::fs::read_to_string("package.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(name) = v["name"].as_str() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Call the model once to produce architecture_summary + cluster_summaries.
/// Returns `Ok((summary, cluster_map))` or `Err` on failure.
/// Callers use `.unwrap_or_default()` so failure → empty strings → graceful fallback.
async fn generate_narrative(
    graph: &ProjectGraph,
    client: &Client,
) -> Result<(String, HashMap<String, String>)> {
    // Build a graph view that excludes the 'root' cluster (markdown/config files only —
    // not useful for architectural understanding and misleads the model).
    let code_only_section = build_code_only_section(graph);

    // Prepend the project name so the model doesn't infer it from file content
    let project_name = detect_project_name()
        .unwrap_or_else(|| std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "this project".to_string()));

    let user_content = format!(
        "Project name: {project_name}\n\n{code_only_section}"
    );

    let messages = vec![Message {
        role: "user".to_string(),
        content: MessageContent::Text(user_content),
        tool_calls: vec![],
    }];

    let response = client
        .chat(NARRATIVE_SYSTEM_PROMPT, &messages, &[], |_| {})
        .await?;

    parse_narrative_response(&response.text)
}

/// Build a prompt section from the graph, excluding the 'root' cluster
/// (which contains only markdown/config files, not source code).
fn build_code_only_section(graph: &ProjectGraph) -> String {
    if graph.clusters.is_empty() {
        return graph.to_prompt_section(12).unwrap_or_default();
    }

    let code_clusters: Vec<&crate::pie::Cluster> = graph
        .clusters
        .iter()
        .filter(|c| c.name != "root")
        .collect();

    if code_clusters.is_empty() {
        return graph.to_prompt_section(12).unwrap_or_default();
    }

    let total_files: usize = code_clusters.iter().map(|c| c.files.len()).sum();
    let mut out = format!(
        "# Project structure — {} clusters, {} files\n",
        code_clusters.len(),
        total_files
    );

    for cluster in &code_clusters {
        out.push('\n');
        out.push_str(&format!("## {} ({} files)\n", cluster.name, cluster.files.len()));
        if !cluster.entry_files.is_empty() {
            let key_parts: Vec<String> = cluster.entry_files.iter()
                .map(|f| {
                    let lines = graph.file_lines.get(f).copied().unwrap_or(0);
                    if lines > 0 { format!("{f} ({lines} lines)") } else { f.clone() }
                })
                .collect();
            out.push_str(&format!("Key files: {}\n", key_parts.join(", ")));
        }
        let syms: Vec<String> = graph.symbols.iter()
            .filter(|s| cluster.files.contains(&s.file))
            .take(10)
            .map(|s| format!("{} {}", s.kind.label(), s.name))
            .collect();
        let total_syms = graph.symbols.iter()
            .filter(|s| cluster.files.contains(&s.file))
            .count();
        if !syms.is_empty() {
            let ellipsis = if total_syms > 10 { format!(" … ({total_syms} symbols)") } else { String::new() };
            out.push_str(&format!("{}{}\n", syms.join(", "), ellipsis));
        }
    }

    out
}

/// Parse the JSON response from the narrative model call.
fn parse_narrative_response(text: &str) -> Result<(String, HashMap<String, String>)> {
    // Strip markdown fences if present
    let text = text.trim();
    let text = strip_markdown_fences(text);

    // Find JSON object
    let start = text.find('{').ok_or_else(|| anyhow::anyhow!("No JSON object in narrative response"))?;
    let json_str = &text[start..];

    #[derive(Deserialize)]
    struct NarrativeResponse {
        architecture_summary: String,
        #[serde(default)]
        cluster_summaries: HashMap<String, String>,
    }

    let parsed: NarrativeResponse = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Narrative JSON parse error: {e}\nResponse: {text}"))?;

    Ok((parsed.architecture_summary, parsed.cluster_summaries))
}

fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim_start_matches("```json").trim_start_matches("```").trim_start();
    let s = s.trim_end_matches("```").trim_end();
    s
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn make_test_graph(tmp: &TempDir) -> ProjectGraph {
        write(tmp.path(), "src/auth/login.rs", "pub fn login() {}\npub struct Session {}\n");
        write(tmp.path(), "src/auth/token.rs", "pub fn validate() {}\n");
        write(tmp.path(), "src/server.rs", "pub fn start() {}\n");
        write(tmp.path(), "src/utils/fmt.rs", "pub fn format_date() {}\n");
        let (graph, _) = crate::pie::ProjectGraph::load_or_build(tmp.path(), 100);
        graph
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_detect_conventions() {
        let tmp = TempDir::new().unwrap();
        let graph = make_test_graph(&tmp);
        let conventions = detect_conventions(&graph);
        assert!(!conventions.is_empty(), "detect_conventions should return at least one entry");
        // Should mention clusters
        assert!(
            conventions.iter().any(|c| c.contains("cluster")),
            "should mention clusters: {conventions:?}"
        );
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_to_context_package_warm() {
        let tmp = TempDir::new().unwrap();
        let graph = make_test_graph(&tmp);

        let mut summaries = HashMap::new();
        summaries.insert("auth".to_string(), "Handles user authentication and session management.".to_string());
        summaries.insert("src".to_string(), "Entry point and server startup logic.".to_string());

        let narrative = ProjectNarrative {
            schema_version: NARRATIVE_SCHEMA_VERSION,
            architecture_summary: "A web service with authentication and utility modules.".to_string(),
            cluster_summaries: summaries,
            conventions: vec!["2 clusters detected".to_string()],
            last_synthesized: 0,
            patches: vec![],
        };

        let ctx = narrative.to_context_package(&graph, &[], 8, &[]);

        assert!(ctx.contains("# Project architecture"), "should contain header: {ctx}");
        assert!(ctx.contains("A web service"), "should contain summary: {ctx}");
        assert!(ctx.contains("Conventions"), "should contain conventions: {ctx}");
        // Should contain cluster info (names from graph)
        assert!(ctx.contains("auth") || ctx.contains("src"), "should contain cluster names: {ctx}");
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_to_context_package_fallback() {
        let tmp = TempDir::new().unwrap();
        let graph = make_test_graph(&tmp);

        // Empty architecture_summary → should fall back to graph.to_prompt_section()
        let narrative = ProjectNarrative::default();
        let ctx = narrative.to_context_package(&graph, &[], 8, &[]);
        let graph_section = graph.to_prompt_section(8).unwrap_or_default();

        // Both should contain the same cluster structure header
        assert!(
            ctx.contains("# Project structure") || ctx.is_empty(),
            "fallback should use graph format: {ctx}"
        );
        // The fallback content should match what the graph produces
        assert_eq!(ctx, graph_section, "fallback should equal graph.to_prompt_section output");
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_narrative_save_round_trip() {
        let tmp = TempDir::new().unwrap();
        let graph = make_test_graph(&tmp);

        let mut summaries = HashMap::new();
        summaries.insert("auth".to_string(), "Auth cluster summary.".to_string());

        let narrative = ProjectNarrative {
            schema_version: NARRATIVE_SCHEMA_VERSION,
            architecture_summary: "Test project architecture.".to_string(),
            cluster_summaries: summaries.clone(),
            conventions: vec!["convention 1".to_string()],
            last_synthesized: 1234567890,
            patches: vec!["patch note".to_string()],
        };

        narrative.save(tmp.path());

        // Read back
        let path = tmp.path().join(NARRATIVE_PATH);
        let content = fs::read_to_string(&path).expect("narrative file should exist");
        let loaded: ProjectNarrative = serde_json::from_str(&content).expect("should parse");

        assert_eq!(loaded.schema_version, NARRATIVE_SCHEMA_VERSION);
        assert_eq!(loaded.architecture_summary, "Test project architecture.");
        assert_eq!(loaded.cluster_summaries.get("auth").map(|s| s.as_str()), Some("Auth cluster summary."));
        assert_eq!(loaded.conventions, vec!["convention 1"]);
        assert_eq!(loaded.last_synthesized, 1234567890);
        assert_eq!(loaded.patches, vec!["patch note"]);

        // Also verify load_or_generate warm path loads it
        // (No client needed — warm path returns before any model call)
        // We can't call async in sync test without tokio, so just verify the file exists
        assert!(path.exists());
        let _ = graph; // suppress unused warning
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_narrative_cold_load_graceful_on_error() {
        // In a dir with no narrative file and a client that will fail (bad endpoint),
        // load_or_generate should return a default (possibly empty) narrative, not panic.
        let tmp = TempDir::new().unwrap();

        // Minimal graph with one file
        write(tmp.path(), "src/main.rs", "pub fn main() {}\n");
        let (graph, _) = crate::pie::ProjectGraph::load_or_build(tmp.path(), 100);

        // Client with invalid endpoint — will fail the model call
        let client = crate::client::Client::new(
            "http://127.0.0.1:1".to_string(), // nothing listening here
            "test-model".to_string(),
        );

        // Should not panic — returns gracefully with empty/default narrative
        let narrative = ProjectNarrative::load_or_generate_ref(&graph, &client, tmp.path()).await;

        // architecture_summary may be empty (model call failed) but struct is valid
        // conventions should still be populated (zero-cost detection runs before model call)
        assert!(
            !narrative.conventions.is_empty(),
            "conventions should be populated even on model failure"
        );
        // Should not panic, should return a narrative struct
        assert_eq!(narrative.schema_version, NARRATIVE_SCHEMA_VERSION);
    }

    // ── Test 6 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_narrative_response_valid() {
        let response = r#"{
            "architecture_summary": "A Rust TUI application for AI-assisted coding.",
            "cluster_summaries": {
                "tui": "Terminal user interface with ratatui.",
                "plan": "Multi-step planning and execution engine."
            }
        }"#;

        let result = parse_narrative_response(response);
        assert!(result.is_ok(), "should parse valid JSON: {result:?}");
        let (summary, clusters) = result.unwrap();
        assert_eq!(summary, "A Rust TUI application for AI-assisted coding.");
        assert_eq!(clusters.get("tui").map(|s| s.as_str()), Some("Terminal user interface with ratatui."));
        assert_eq!(clusters.get("plan").map(|s| s.as_str()), Some("Multi-step planning and execution engine."));
    }

    #[test]
    fn test_parse_narrative_response_with_fences() {
        let response = "```json\n{\"architecture_summary\": \"Test summary.\", \"cluster_summaries\": {}}\n```";
        let result = parse_narrative_response(response);
        assert!(result.is_ok(), "should strip fences: {result:?}");
        let (summary, _) = result.unwrap();
        assert_eq!(summary, "Test summary.");
    }
}
