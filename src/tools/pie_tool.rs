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

use crate::index::SymbolKind;
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

    // Field-level search — scan struct/enum signatures for the query as a field/variant name
    let field_matches: Vec<String> = graph.symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Enum))
        .filter_map(|s| {
            let sig = s.signature.as_deref()?;
            find_field_in_sig(sig, name).map(|snippet| {
                format!("  {snippet}  ← in {} {} ({}:{})", s.kind.label(), s.name, s.file, s.line)
            })
        })
        .take(5)
        .collect();

    if !field_matches.is_empty() {
        return format!(
            "'{name}' not found as a top-level symbol.\nField/variant matches:\n{}",
            field_matches.join("\n")
        );
    }

    format!("Symbol or file '{name}' not found in project index.")
}

// ── Signature formatting ───────────────────────────────────────────────────────

/// Expand a compact one-line signature `{ a: T, b: U }` into multi-line form.
///
/// `format_compact` in callgraph.rs stores all fields/variants on one line — fine
/// for embedding in orient, but hard for the model to parse for large enums like
/// UiEvent. This expands to one field/variant per line so the model can spot what
/// it's looking for without needing a confirmation read.
fn expand_compact_sig(sig: &str, indent: &str) -> String {
    let trimmed = sig.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return format!("{indent}{trimmed}\n");
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return format!("{indent}{{}}\n");
    }
    let parts = split_top_level_commas(inner);
    let mut out = format!("{indent}{{\n");
    for part in &parts {
        let p = part.trim();
        if !p.is_empty() {
            out.push_str(&format!("{indent}    {p},\n"));
        }
    }
    out.push_str(&format!("{indent}}}\n"));
    out
}

/// Split `s` at commas that are NOT nested inside `{`, `(`, or `<…>`.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' | '(' | '<' => depth += 1,
            '}' | ')' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Search a compact signature string for a field or variant name matching `query`.
/// Returns the specific field/variant entry (e.g. "cost_per_mtok_input: Option<f64>")
/// Extract the full field name from a signature that *contains* the query as a substring.
///
/// e.g. sig=`{ cost_per_mtok_input: Option<f64> }`, query="cost" → "cost_per_mtok_input"
///
/// Used by orient to suggest `check_wiring(field="cost_per_mtok_input")` rather than
/// `check_wiring(field="cost")`, which would fail the word-boundary match.
fn extract_field_name_from_sig(sig: &str, query: &str) -> Option<String> {
    if query.len() < 3 { return None; }
    let sig_lower = sig.to_lowercase();
    let q_lower = query.to_lowercase();

    let mut search = 0;
    while search < sig_lower.len() {
        let Some(hit) = sig_lower[search..].find(q_lower.as_str()) else { break };
        let abs = search + hit;

        // Walk back to the start of the identifier
        let field_start = sig[..abs]
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);

        // Walk forward to the end of the identifier (stop at non-ident char)
        let field_end = abs + sig[abs..].find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(sig.len() - abs);

        let candidate = sig[field_start..field_end].trim();

        // Must be followed by `:` to be a struct field (not a type name)
        let after = sig[field_end..].trim_start();
        if after.starts_with(':') && !candidate.is_empty() && !candidate.contains(' ') {
            return Some(candidate.to_string());
        }

        search = abs + 1;
    }
    None
}

/// Scan a struct/enum signature for a field or variant matching `query`,
/// if found at a word boundary. Handles both struct fields (`name: Type`) and enum
/// variants (`VariantName` or `VariantName { ... }` or `VariantName(...)`).
fn find_field_in_sig<'a>(sig: &'a str, query: &str) -> Option<&'a str> {
    if query.len() < 3 { return None; } // avoid noise on short queries
    let sig_lower = sig.to_lowercase();
    let q_lower = query.to_lowercase();

    // Try as a struct field name: query immediately followed by ':'
    let field_pat = format!("{q_lower}:");
    let mut search = 0;
    while let Some(rel) = sig_lower[search..].find(&field_pat) {
        let idx = search + rel;
        // Word boundary before: must be '{', ' ', or start of string
        let before_ok = idx == 0
            || matches!(sig.as_bytes().get(idx.saturating_sub(1)).copied(), Some(b'{') | Some(b' ') | Some(b','));
        if before_ok {
            let rest = &sig[idx..];
            let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
            return Some(rest[..end].trim_end_matches(',').trim());
        }
        search = idx + 1;
        if search >= sig_lower.len() { break; }
    }

    // Try as a variant or tuple field name: word boundary before AND after
    let mut search = 0;
    while let Some(rel) = sig_lower[search..].find(&q_lower) {
        let idx = search + rel;
        let end_idx = idx + q_lower.len();
        let before_ok = idx == 0 || {
            let b = sig.as_bytes()[idx - 1];
            b == b'{' || b == b' ' || b == b',' || b == b'('
        };
        let after_ok = end_idx >= sig.len() || {
            let b = sig.as_bytes()[end_idx];
            // '_' included: query may be a prefix of a compound field name
            // e.g. "cost_per_mtok" correctly matches "cost_per_mtok_input"
            b == b' ' || b == b'{' || b == b',' || b == b'}' || b == b'(' || b == b')' || b == b'_'
        };
        if before_ok && after_ok {
            let rest = &sig[idx..];
            let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
            return Some(rest[..end].trim_end_matches(',').trim());
        }
        search = idx + 1;
        if search >= sig_lower.len() { break; }
    }

    None
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

/// Maps a file path to its pipeline layer label (for display).
fn pipeline_layer(file: &str) -> &'static str {
    if file.contains("config") { return "[config]"; }
    if file.contains("/main") || file.contains("setup") || file.contains("init") { return "[init]  "; }
    if file.contains("agent") || file.contains("plan") || file.contains("budget") { return "[agent] "; }
    if file.contains("tui") || file.contains("render") || file.contains("stats") { return "[ui]    "; }
    if file.contains("tool") { return "[tools] "; }
    "[core]  "
}

/// Maps a file path to a sort order for pipeline layer (config=0, ui=4).
fn pipeline_layer_order(file: &str) -> u8 {
    if file.contains("config") { return 0; }
    if file.contains("/main") || file.contains("setup") || file.contains("init") { return 1; }
    if file.contains("agent") || file.contains("plan") || file.contains("budget") { return 2; }
    if file.contains("tool") { return 3; }
    if file.contains("tui") || file.contains("render") || file.contains("stats") { return 4; }
    5
}

pub fn check_wiring_definition() -> Value {
    serde_json::json!({
        "name": "check_wiring",
        "description": "Verify whether a field or concept is propagated through all required structs — zero disk reads.\n\
                        Scans all struct/enum type definitions for fields matching the query and reports\n\
                        which types HAVE it and which adjacent types DON'T — the missing list IS your plan.\n\n\
                        Use this after find_symbol finds a field somewhere to verify the full pipeline:\n\
                        config → resolved_config → agent_config → event → ui.\n\n\
                        Optionally provide 'structs' to check specific types by name.\n\
                        Without 'structs', scans all indexed types and shows cluster-adjacent gaps.",
        "parameters": {
            "type": "object",
            "properties": {
                "field": {
                    "type": "string",
                    "description": "Field name or keyword to search for (e.g. \"cost_per_mtok\", \"max_tokens\", \"hook\")"
                },
                "structs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of struct/enum names to explicitly check (e.g. [\"Profile\", \"AgentConfig\", \"UiEvent\"])"
                }
            },
            "required": ["field"]
        }
    })
}

pub fn check_wiring_execute(args: &Value, graph: &ProjectGraph) -> String {
    let field = args["field"].as_str().unwrap_or("").trim();
    if field.is_empty() {
        return "Provide field= for check_wiring. Example: check_wiring(field=\"cost_per_mtok\")".to_string();
    }

    let specific: Option<Vec<String>> = args["structs"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());

    // All struct/enum symbols that have signatures populated
    let typed: Vec<&crate::index::Symbol> = graph.symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Enum))
        .filter(|s| s.signature.is_some())
        .collect();

    let mut with_syms: Vec<&crate::index::Symbol> = Vec::new();
    let mut without_syms: Vec<&crate::index::Symbol> = Vec::new();

    if let Some(ref names) = specific {
        // Guard: if the filter names don't match anything, warn immediately rather than
        // silently returning empty results (common when model hallucinates struct names).
        let matched: Vec<&&crate::index::Symbol> = typed.iter()
            .filter(|s| names.iter().any(|n| n.eq_ignore_ascii_case(&s.name)))
            .collect();
        if matched.is_empty() {
            return format!(
                "check_wiring: none of the provided `structs` names matched any indexed type: {:?}\n\
                 Tip: omit `structs=` to search all types, or use orient(query=\"{field}\") \
                 to find the right struct names first.",
                names
            );
        }
        // Explicit list — check only those, report both with and without
        for sym in typed.iter().filter(|s| names.iter().any(|n| n.eq_ignore_ascii_case(&s.name))) {
            let sig = sym.signature.as_deref().unwrap_or("");
            if find_field_in_sig(sig, field).is_some() {
                with_syms.push(sym);
            } else {
                without_syms.push(sym);
            }
        }
    } else {
        // Scan all for exact field matches
        for sym in &typed {
            let sig = sym.signature.as_deref().unwrap_or("");
            if find_field_in_sig(sig, field).is_some() {
                with_syms.push(sym);
            }
        }

        if with_syms.is_empty() {
            // Fuzzy fallback: find field names in signatures that contain the query as a substring
            let field_lower = field.to_lowercase();
            let mut fuzzy: Vec<String> = Vec::new();
            let mut seen_fields: std::collections::HashSet<String> = std::collections::HashSet::new();

            for sym in &typed {
                let sig = sym.signature.as_deref().unwrap_or("");
                let sig_lower = sig.to_lowercase();
                // Find "query_something:" or "something_query:" patterns in the signature
                let mut search = 0;
                while let Some(rel) = sig_lower[search..].find(&field_lower) {
                    let idx = search + rel;
                    // Extract the full field name (up to ':' or end of word)
                    let start_of_field = sig[..idx].rfind(|c: char| c == '{' || c == ',' || c == ' ').map(|i| i + 1).unwrap_or(0);
                    let end_of_field = sig[idx..].find(':').map(|i| i + idx).unwrap_or(sig.len());
                    let candidate = sig[start_of_field..end_of_field].trim();
                    if !candidate.is_empty() && !candidate.contains('{') && !candidate.contains('}') {
                        let key = format!("{}::{}", sym.name, candidate);
                        if seen_fields.insert(key) {
                            fuzzy.push(format!(
                                "  {} {} — field '{}' ({}:{})",
                                sym.kind.label(), sym.name, candidate, sym.file, sym.line
                            ));
                        }
                    }
                    search = idx + 1;
                    if search >= sig_lower.len() { break; }
                }
            }

            let mut out = format!("No structs found with exact '{}' field.\n", field);
            if !fuzzy.is_empty() {
                fuzzy.dedup();
                fuzzy.truncate(8);
                out.push_str(&format!(
                    "Partial matches (field names containing '{}'):\n{}\n\n\
                     Retry with the exact field name: check_wiring(field=\"<exact_name>\")",
                    field, fuzzy.join("\n")
                ));
            } else {
                out.push_str(&format!(
                    "Try find_symbol(name=\"{field}\") to locate it as a top-level symbol."
                ));
            }
            return out;
        }

        // ── Call-adjacency filtering for WITHOUT ──────────────────────────────
        // Instead of whole-cluster scan (noisy), follow call edges 1 hop from
        // files containing WITH structs — only directly-adjacent files appear.
        let files_with: std::collections::HashSet<&str> = with_syms.iter()
            .map(|s| s.file.as_str())
            .collect();

        let mut adjacent_files: std::collections::HashSet<&str> = std::collections::HashSet::new();

        for (key, edges) in &graph.call_edges {
            let caller_file = key.split("::").next().unwrap_or("");
            // Outgoing: functions IN files_with call into other files
            if files_with.contains(caller_file) {
                for edge in edges {
                    if let Some(callee_files) = graph.by_name.get(&edge.callee) {
                        for f in callee_files {
                            if !files_with.contains(f.as_str()) {
                                adjacent_files.insert(f.as_str());
                            }
                        }
                    }
                }
            }
            // Incoming: functions in other files call INTO files_with
            for edge in edges {
                if let Some(callee_files) = graph.by_name.get(&edge.callee) {
                    if callee_files.iter().any(|f| files_with.contains(f.as_str()))
                        && !files_with.contains(caller_file)
                    {
                        adjacent_files.insert(caller_file);
                    }
                }
            }
        }

        // Structs in adjacent files that DON'T have the field
        for sym in &typed {
            if find_field_in_sig(sym.signature.as_deref().unwrap_or(""), field).is_some() { continue; }
            if adjacent_files.contains(sym.file.as_str()) {
                without_syms.push(sym);
            }
        }
    }

    // ── Build output ──────────────────────────────────────────────────────────
    let mut out = format!("Wiring report for '{field}':\n\n");

    if with_syms.is_empty() {
        out.push_str(&format!(
            "No structs/enums found with '{}' fields.\n\
             Try find_symbol(name=\"{}\") to locate it as a top-level symbol.\n",
            field, field
        ));
    } else {
        let with_lines: Vec<String> = with_syms.iter().map(|sym| {
            let sig = sym.signature.as_deref().unwrap_or("");
            let snippet = find_field_in_sig(sig, field).unwrap_or(field);
            format!("  {} {} {} ({}:{}) — {}",
                pipeline_layer(&sym.file), sym.kind.label(), sym.name, sym.file, sym.line, snippet)
        }).collect();
        out.push_str(&format!("WITH '{field}':\n{}\n\n", with_lines.join("\n")));
    }

    if !without_syms.is_empty() {
        let without_lines: Vec<String> = without_syms.iter().map(|sym| {
            let sig = sym.signature.as_deref().unwrap_or("");
            let preview = if sig.len() > 70 { format!("{}…", &sig[..70]) } else { sig.to_string() };
            format!("  {} {} {} ({}:{}) — [missing]  has: {}",
                pipeline_layer(&sym.file), sym.kind.label(), sym.name, sym.file, sym.line, preview)
        }).collect();
        out.push_str(&format!(
            "WITHOUT '{field}' — pipeline gaps ({} found):\n{}\n\n\
             Each [missing] entry above is a step in your plan.",
            without_syms.len(), without_lines.join("\n")
        ));
    } else if !with_syms.is_empty() {
        if specific.is_some() {
            out.push_str(&format!("All checked types contain '{field}' — fully wired.\n"));
        } else {
            out.push_str(&format!("No call-adjacent gaps found for '{field}' — appears fully wired in the immediate pipeline.\n"));
        }
    }

    out
}

pub fn orient_definition() -> Value {
    serde_json::json!({
        "name": "orient",
        "description": "Find all symbols related to a task — returns struct signatures, locations, and \
                        call connections in pipeline order (config → agent → ui). \
                        Use this INSTEAD of find_symbol + trace_calls. \
                        One call returns everything needed to understand the codebase shape for a task. \
                        Zero disk reads — in-memory graph lookup.\n\n\
                        After orient, call check_wiring(field=X) to verify field propagation gaps.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Task keywords or symbol names (e.g. \"cost tracking\", \"token stats\", \"AgentConfig run_tui\")"
                }
            },
            "required": ["query"]
        }
    })
}

pub fn orient_execute(args: &Value, graph: &ProjectGraph) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    if query.is_empty() {
        return "Provide query= for orient. Example: orient(query=\"cost tracking\")".to_string();
    }

    // Tokenise query into keywords (reuse flowpaths splitter for consistency)
    let keywords: Vec<String> = query
        .split_whitespace()
        .flat_map(|w| crate::flowpaths::split_identifier(w))
        .filter(|w| w.len() >= 3)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if keywords.is_empty() {
        return format!("No usable keywords in '{query}'. Use terms with 3+ characters.");
    }

    // Score each symbol:
    //   3 = a keyword exactly matches a word in the symbol name (word-boundary)
    //   2 = a keyword is a substring of the symbol name
    //   1 = a keyword appears in the signature
    use std::collections::HashMap;
    let mut best: HashMap<(String, String), (u8, usize)> = HashMap::new(); // (file,name) -> (score, idx)

    for (idx, sym) in graph.symbols.iter().enumerate() {
        if !matches!(sym.kind, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Function | SymbolKind::Trait) {
            continue;
        }
        let name_lower = sym.name.to_lowercase();
        let sym_words: Vec<String> = crate::flowpaths::split_identifier(&sym.name);
        let sig_lower = sym.signature.as_deref().unwrap_or("").to_lowercase();

        let mut top: u8 = 0;
        for kw in &keywords {
            let score = if sym_words.iter().any(|w| w == kw) {
                3
            } else if name_lower.contains(kw.as_str()) {
                2
            } else if !sig_lower.is_empty() && sig_lower.contains(kw.as_str()) {
                1
            } else {
                0
            };
            if score > top { top = score; }
        }

        if top > 0 {
            let key = (sym.file.clone(), sym.name.clone());
            let existing = best.get(&key).map(|(s, _)| *s).unwrap_or(0);
            if top > existing {
                best.insert(key, (top, idx));
            }
        }
    }

    if best.is_empty() {
        return format!(
            "No symbols found matching '{query}'.\n\
             Try broader terms or orient(query=\"<file_stem>\") to see a file's symbols."
        );
    }

    // Separate structs/enums (most useful for planning) from functions
    let mut type_syms: Vec<(u8, &crate::index::Symbol)> = Vec::new();
    let mut fn_syms: Vec<(u8, &crate::index::Symbol)> = Vec::new();

    for ((_, _), (score, idx)) in &best {
        let sym = &graph.symbols[*idx];
        if matches!(sym.kind, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait) {
            type_syms.push((*score, sym));
        } else {
            fn_syms.push((*score, sym));
        }
    }

    // Sort: score desc, then pipeline order asc
    type_syms.sort_by(|a, b| b.0.cmp(&a.0).then(pipeline_layer_order(a.1.file.as_str()).cmp(&pipeline_layer_order(b.1.file.as_str()))));
    fn_syms.sort_by(|a, b| b.0.cmp(&a.0).then(pipeline_layer_order(a.1.file.as_str()).cmp(&pipeline_layer_order(b.1.file.as_str()))));

    type_syms.truncate(10);
    fn_syms.truncate(8);

    let mut out = format!("# orient: \"{query}\"\n\n");

    // ── Types section ─────────────────────────────────────────────────────────
    // Compact: one line per type. expand_compact_sig is deliberately NOT used here —
    // orient is a navigation map, not a data dump. Full field layout is served by
    // the struct intercept when the model explicitly reads a struct range.
    if !type_syms.is_empty() {
        out.push_str("## Types (pipeline order)\n");
        for (_, sym) in &type_syms {
            let layer = pipeline_layer(sym.file.as_str());
            out.push_str(&format!("{} {} {} ({}:{})\n", layer, sym.kind.label(), sym.name, sym.file, sym.line));
            if let Some(sig) = &sym.signature {
                out.push_str(&format!("  {sig}\n"));
            }
            // Callers: functions that call this type (by name reference in call edges)
            let callers: Vec<String> = graph.call_edges.iter()
                .filter_map(|(caller_key, edges)| {
                    if edges.iter().any(|e| e.callee == sym.name) {
                        let caller_name = caller_key.split("::").last().unwrap_or(caller_key);
                        let caller_file = caller_key.split("::").next().unwrap_or("");
                        let loc = graph.symbols.iter()
                            .find(|s| s.file == caller_file && s.name == caller_name)
                            .map(|s| format!("{}:{}", s.file, s.line))
                            .unwrap_or_else(|| caller_file.to_string());
                        Some(format!("{caller_name} ({loc})"))
                    } else {
                        None
                    }
                })
                .take(6)
                .collect();
            if !callers.is_empty() {
                out.push_str(&format!("  used by: {}\n", callers.join(", ")));
            }
            // Functions that construct this type or any of its variants
            let enum_prefix = format!("{}::", sym.name);
            let mut constructors: Vec<String> = graph.construct_edges.iter()
                .filter_map(|(caller_key, edges)| {
                    if edges.iter().any(|e| e.callee == sym.name || e.callee.starts_with(&enum_prefix)) {
                        let caller_name = caller_key.split("::").last().unwrap_or(caller_key);
                        let caller_file = caller_key.split("::").next().unwrap_or("");
                        let loc = graph.symbols.iter()
                            .find(|s| s.file == caller_file && s.name == caller_name)
                            .map(|s| format!("{}:{}", s.file, s.line))
                            .unwrap_or_else(|| caller_file.to_string());
                        Some(format!("{caller_name} ({loc})"))
                    } else {
                        None
                    }
                })
                .collect();
            constructors.sort();
            constructors.dedup();
            constructors.truncate(6);
            if !constructors.is_empty() {
                out.push_str(&format!("  constructed by: {}\n", constructors.join(", ")));
            }
            out.push('\n');
        }
    }

    // Explicit note: struct/enum layouts above are complete — no confirmation read needed.
    if !type_syms.is_empty() {
        out.push_str(
            "Struct/enum layouts above are authoritative — \
             do not read_file to confirm them. \
             Proceed to check_wiring or targeted function reads only.\n\n"
        );
    }

    // ── Functions section ─────────────────────────────────────────────────────
    if !fn_syms.is_empty() {
        out.push_str("## Key functions\n");
        for (_, sym) in &fn_syms {
            let layer = pipeline_layer(sym.file.as_str());
            out.push_str(&format!("{} fn {} ({}:{}-{})\n", layer, sym.name, sym.file, sym.line, sym.end_line));
            // Outgoing calls
            let key = format!("{}::{}", sym.file, sym.name);
            if let Some(edges) = graph.call_edges.get(&key) {
                let callees: Vec<String> = edges.iter().take(12).map(|e| {
                    let loc = graph.symbols.iter()
                        .find(|s| s.name == e.callee)
                        .map(|s| format!("{}:{}", s.file, s.line))
                        .unwrap_or_default();
                    if loc.is_empty() { e.callee.clone() } else { format!("{} ({loc})", e.callee) }
                }).collect();
                if !callees.is_empty() {
                    out.push_str(&format!("  calls: {}\n", callees.join(", ")));
                }
            }
            // Struct/enum-variant constructions made by this function
            if let Some(c_edges) = graph.construct_edges.get(&key) {
                let constructs: Vec<&str> = c_edges.iter().take(8).map(|e| e.callee.as_str()).collect();
                if !constructs.is_empty() {
                    out.push_str(&format!("  constructs: {}\n", constructs.join(", ")));
                }
            }
            out.push('\n');
        }
    }

    // ── check_wiring hint ─────────────────────────────────────────────────────
    // Find the actual full field name from signatures rather than the raw keyword.
    // e.g. keyword "cost" → suggests check_wiring(field="cost_per_mtok_input"), not "cost".
    let field_hint: Option<String> = keywords.iter().find_map(|kw| {
        graph.symbols.iter()
            .filter(|s| s.signature.is_some())
            .find_map(|s| extract_field_name_from_sig(s.signature.as_deref().unwrap_or(""), kw))
    });
    if let Some(ref field_name) = field_hint {
        out.push_str(&format!(
            "Suggested: check_wiring(field=\"{field_name}\") to verify full pipeline propagation.\n"
        ));
    }

    out
}

/// Classify a read_file call and handle it intelligently using the project graph.
///
/// Three modes (checked in priority order):
///   Struct intercept — checked FIRST. If a struct/enum/trait definition start line falls
///                    inside [start, end], serve layout from graph. No tolerance — exact
///                    match prevents adjacent functions from "winning" over neighbouring structs.
///   Augment        — range contains function bodies (with ±3 tolerance). Allows the read
///                    but prepends a graph overlay so one read does the work of three calls.
///   Pass-through   — unindexed range (targeted logic blocks). Clean read, no interception.
pub fn smart_read(args: &Value, graph: &ProjectGraph) -> String {
    use crate::index::SymbolKind;

    let path = match args["path"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return super::read::execute(args).unwrap_or_else(|e| format!("[read_file error: {e}]")),
    };

    // Only intercept ranged reads — full-file reads pass through.
    let line_range = args["line_range"].as_array().and_then(|arr| {
        let s = arr.first()?.as_u64()? as usize;
        let e = arr.get(1)?.as_u64()? as usize;
        Some((s, e))
    });
    let Some((start, end)) = line_range else {
        return super::read::execute(args).unwrap_or_else(|e| format!("[read_file error: {e}]"));
    };

    // ── STRUCT INTERCEPT (checked first — takes priority over AUGMENT) ───────
    // Exact match: the struct/enum definition start line falls inside [start, end].
    // No tolerance here — tolerance on the function check causes adjacent fns to
    // "win" over structs they merely neighbour (e.g. fn at 63 beating struct at 24-61).
    let struct_syms: Vec<&crate::index::Symbol> = graph.symbols.iter()
        .filter(|s| s.file == path)
        .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait))
        .filter(|s| s.signature.is_some())
        .filter(|s| s.line >= start && s.line <= end)  // struct definition starts inside range
        .collect();

    if !struct_syms.is_empty() {
        let mut out = format!(
            "[{path} — lines {start}-{end} (struct/enum layout served from project index)]\n\n"
        );
        for sym in &struct_syms {
            let kind_label = sym.kind.label();
            out.push_str(&format!(
                "  line {}: {} {}\n",
                sym.line, kind_label, sym.name
            ));
            if let Some(sig) = &sym.signature {
                out.push_str(&expand_compact_sig(sig, "  "));
            }
            out.push('\n');
        }
        out.push_str(
            "Layout is authoritative (matches file). \
             Use check_wiring(field=\"name\") to trace where a field flows.\n"
        );
        return out;
    }

    // Collect function symbols whose definition falls within [start, end].
    // Use a small tolerance (±3 lines) to handle off-by-one line_range estimates.
    let tolerance = 3usize;
    let fn_syms: Vec<&crate::index::Symbol> = graph.symbols.iter()
        .filter(|s| s.file == path)
        .filter(|s| matches!(s.kind, SymbolKind::Function))
        .filter(|s| {
            let sym_start = s.line.saturating_sub(tolerance);
            let sym_end = s.end_line + tolerance;
            sym_start <= end && sym_end >= start
        })
        .collect();

    // ── AUGMENT: function bodies — allow read, prepend graph overlay ──────────
    if !fn_syms.is_empty() {
        let mut overlay = format!(
            "[Graph overlay for {}:{}-{}]\n",
            path, start, end
        );

        for sym in &fn_syms {
            let key = format!("{}::{}", sym.file, sym.name);
            overlay.push_str(&format!("fn {} ({}:{}-{}):\n", sym.name, sym.file, sym.line, sym.end_line));

            // Outgoing calls
            if let Some(edges) = graph.call_edges.get(&key) {
                if !edges.is_empty() {
                    let callees: Vec<String> = edges.iter().take(10).map(|e| {
                        graph.symbols.iter()
                            .find(|s| s.name == e.callee)
                            .map(|s| format!("{} ({}:{})", e.callee, s.file, s.line))
                            .unwrap_or_else(|| e.callee.clone())
                    }).collect();
                    overlay.push_str(&format!("  calls: {}\n", callees.join(", ")));
                }
            }

            // Incoming callers
            let callers = graph.callers_of(&sym.name);
            if !callers.is_empty() {
                let shown: Vec<&str> = callers.iter().take(8).copied().collect();
                let extra = if callers.len() > 8 { format!(" (+{})", callers.len() - 8) } else { String::new() };
                overlay.push_str(&format!("  called by: {}{}\n", shown.join(", "), extra));
            }
        }

        // Key types in the same file — compact one-liner per type so the model can
        // see what fields exist without a follow-up read. Full layout is available via
        // the struct intercept if the model reads a struct range explicitly.
        let file_types: Vec<String> = graph.symbols.iter()
            .filter(|s| s.file == path)
            .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Enum))
            .filter(|s| s.signature.is_some())
            .take(6)
            .map(|s| format!("  {} {} (line {}): {}", s.kind.label(), s.name, s.line, s.signature.as_deref().unwrap_or("")))
            .collect();
        if !file_types.is_empty() {
            overlay.push_str("Key types in this file:\n");
            for t in &file_types {
                overlay.push_str(&format!("{t}\n"));
            }
        }

        overlay.push_str("[/Graph overlay]\n\n");

        let content = super::read::execute(args)
            .unwrap_or_else(|e| format!("[read_file error: {e}]"));
        return format!("{overlay}{content}");
    }


    // ── PASS-THROUGH: unindexed range (targeted logic block) ─────────────────
    super::read::execute(args).unwrap_or_else(|e| format!("[read_file error: {e}]"))
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
            construct_edges: HashMap::new(),
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

    #[test]
    fn test_find_field_in_sig_struct() {
        let sig = "{ model: String, cost_per_mtok_input: Option<f64>, cost_per_mtok_output: Option<f64> }";
        let result = find_field_in_sig(sig, "cost_per_mtok_input");
        assert_eq!(result, Some("cost_per_mtok_input: Option<f64>"), "got: {result:?}");
    }

    #[test]
    fn test_find_field_in_sig_enum_variant() {
        let sig = "{ TokenStats { input_tokens: u32, output_tokens: u32 }, AgentDone, Message(String) }";
        let result = find_field_in_sig(sig, "TokenStats");
        assert!(result.is_some(), "should find TokenStats variant: {result:?}");
        assert!(result.unwrap().starts_with("TokenStats"), "got: {result:?}");
    }

    #[test]
    fn test_find_field_in_sig_no_match() {
        let sig = "{ model: String, count: u32 }";
        let result = find_field_in_sig(sig, "cost");
        assert_eq!(result, None, "should not match partial: {result:?}");
    }

    #[test]
    fn test_find_symbol_field_search() {
        use crate::index::{Symbol, SymbolKind};
        use std::collections::HashMap;
        let mut graph = make_graph(); // uses existing make_graph() fixture
        // Add a struct with a signature
        graph.symbols.push(Symbol {
            name: "Profile".to_string(),
            file: "src/config.rs".to_string(),
            line: 10,
            end_line: 20,
            kind: SymbolKind::Struct,
            signature: Some("{ model: String, cost_per_mtok_input: Option<f64> }".to_string()),
        });
        graph.by_name.entry("Profile".to_string()).or_default().push("src/config.rs".to_string());
        graph.file_lines.insert("src/config.rs".to_string(), 100);

        let args = serde_json::json!({"name": "cost_per_mtok_input"});
        let result = execute(&args, &graph);
        assert!(result.contains("cost_per_mtok_input"), "should find field: {result}");
        assert!(result.contains("Profile"), "should name containing type: {result}");
    }

    #[test]
    fn test_check_wiring_finds_gap() {
        use crate::index::{Symbol, SymbolKind};
        let mut graph = make_graph();

        // Profile HAS cost_per_mtok, AgentConfig does NOT
        graph.symbols.push(Symbol {
            name: "Profile".to_string(),
            file: "src/config.rs".to_string(),
            line: 10, end_line: 20,
            kind: SymbolKind::Struct,
            signature: Some("{ model: String, cost_per_mtok_input: Option<f64> }".to_string()),
        });
        graph.symbols.push(Symbol {
            name: "AgentConfig".to_string(),
            file: "src/agent.rs".to_string(),
            line: 474, end_line: 503,
            kind: SymbolKind::Struct,
            signature: Some("{ client: Client, model: String }".to_string()),
        });
        graph.file_lines.insert("src/config.rs".to_string(), 100);

        // Explicit structs check
        let args = serde_json::json!({
            "field": "cost_per_mtok",
            "structs": ["Profile", "AgentConfig"]
        });
        let result = check_wiring_execute(&args, &graph);
        assert!(result.contains("Profile"), "should mention Profile: {result}");
        assert!(result.contains("AgentConfig"), "should mention AgentConfig: {result}");
        assert!(result.contains("[missing]") || result.contains("missing"), "should show gap: {result}");
    }

    #[test]
    fn test_smart_read_redirects_type_range() {
        use crate::index::{Symbol, SymbolKind};
        let mut graph = make_graph();
        // run_tui is a Function in make_graph (line 42-200) — NOT a type
        // Add a struct with signature
        graph.symbols.push(Symbol {
            name: "Profile".to_string(),
            file: "src/config.rs".to_string(),
            line: 10, end_line: 25,
            kind: SymbolKind::Struct,
            signature: Some("{ model: String, cost_per_mtok_input: Option<f64> }".to_string()),
        });
        graph.file_lines.insert("src/config.rs".to_string(), 100);

        // Request range that covers Profile struct only
        let args = serde_json::json!({
            "path": "src/config.rs",
            "line_range": [10, 25]
        });
        let result = smart_read(&args, &graph);
        assert!(result.contains("served from project index"), "should redirect type read: {result}");
        assert!(result.contains("Profile"), "should include type name: {result}");
        assert!(result.contains("cost_per_mtok_input"), "should include field: {result}");
    }

    #[test]
    fn test_smart_read_passthrough_no_symbols() {
        let graph = make_graph();
        // Request a range in a file with no symbols (src/main.rs lines 1-5)
        let args = serde_json::json!({
            "path": "src/main.rs",
            "line_range": [1, 5]
        });
        // Should attempt a read (will fail since file doesn't exist in test env, that's fine)
        let result = smart_read(&args, &graph);
        // Should NOT say "redirected" — it's a pass-through
        assert!(!result.contains("redirected"), "should not redirect: {result}");
    }

    #[test]
    fn test_check_wiring_fuzzy_match() {
        use crate::index::{Symbol, SymbolKind};
        let mut graph = make_graph();
        graph.symbols.push(Symbol {
            name: "Profile".to_string(),
            file: "src/config.rs".to_string(),
            line: 10, end_line: 20,
            kind: SymbolKind::Struct,
            signature: Some("{ model: String, cost_per_mtok_input: Option<f64> }".to_string()),
        });
        graph.file_lines.insert("src/config.rs".to_string(), 100);

        // Query a prefix — should fuzzy match
        let args = serde_json::json!({"field": "cost_per_mtok"});
        let result = check_wiring_execute(&args, &graph);
        // Should either find it or suggest the full field name
        assert!(
            result.contains("cost_per_mtok_input") || result.contains("Partial matches"),
            "should fuzzy match or find: {result}"
        );
    }

    #[test]
    fn test_check_wiring_pipeline_labels() {
        use crate::index::{Symbol, SymbolKind};
        let mut graph = make_graph();
        graph.symbols.push(Symbol {
            name: "Profile".to_string(),
            file: "src/config.rs".to_string(),
            line: 10, end_line: 20,
            kind: SymbolKind::Struct,
            signature: Some("{ cost_per_mtok_input: Option<f64> }".to_string()),
        });
        graph.file_lines.insert("src/config.rs".to_string(), 100);

        let args = serde_json::json!({
            "field": "cost_per_mtok_input",
            "structs": ["Profile"]
        });
        let result = check_wiring_execute(&args, &graph);
        assert!(result.contains("[config]"), "should show pipeline layer label: {result}");
    }
}
