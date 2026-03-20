/// Tree-sitter call graph extraction for Rust source files.
///
/// Walks the concrete syntax tree of each `.rs` file and records outgoing
/// call edges: for every top-level symbol, which project-internal functions
/// does it call and on which lines?
///
/// Design:
/// - Only Rust for now. Other languages added per grammar crate.
/// - Filters to project-internal calls only (callee must be in `known_names`).
/// - One representative call site per (caller, callee) pair — we care about
///   edge existence and a source line, not every occurrence.
/// - Falls back gracefully: if tree-sitter init fails, extraction is skipped
///   and the graph simply has no call edges (existing behaviour preserved).
use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::index::{CallEdge, Symbol};

// ── Public API ────────────────────────────────────────────────────────────────

/// Stateful extractor.  Create once, reuse across many files (parser is
/// expensive to initialise).
pub struct CallExtractor {
    parser: Parser,
}

impl CallExtractor {
    /// Initialise the tree-sitter Rust parser.
    ///
    /// Returns `Err` if the grammar ABI version is incompatible with the
    /// linked `tree-sitter` crate — in that case callers should skip call
    /// extraction rather than hard-fail.
    pub fn new() -> anyhow::Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rust::language())?;
        Ok(Self { parser })
    }

    /// Extract outgoing call edges from one Rust source file.
    ///
    /// # Arguments
    /// - `content`     — full UTF-8 source of the file
    /// - `file`        — relative path used as key prefix ("src/agent.rs")
    /// - `symbols`     — all project symbols (will be filtered to this file)
    /// - `known_names` — the `by_name` map from `ProjectGraph`; only calls
    ///                   whose callee appears here are kept (project-internal)
    ///
    /// # Returns
    /// `HashMap<String, Vec<CallEdge>>` where the key is `"file::symbol_name"`.
    pub fn extract_file(
        &mut self,
        content: &str,
        file: &str,
        symbols: &[Symbol],
        known_names: &HashMap<String, Vec<String>>,
    ) -> HashMap<String, Vec<CallEdge>> {
        let mut result: HashMap<String, Vec<CallEdge>> = HashMap::new();

        let tree = match self.parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => return result, // parse failed — skip silently
        };

        let src = content.as_bytes();

        // Pre-filter symbols to this file to speed up per-call lookup.
        let file_syms: Vec<&Symbol> = symbols.iter().filter(|s| s.file == file).collect();

        // Collect all raw call sites from the CST.
        let mut raw_calls: Vec<(usize, String)> = Vec::new();
        collect_calls(tree.root_node(), src, &mut raw_calls);

        for (call_line, callee) in raw_calls {
            // Skip calls to external / stdlib symbols.
            if !known_names.contains_key(&callee) {
                continue;
            }

            // Find the top-level symbol that contains this call.
            let Some(sym) = find_containing_symbol(&file_syms, call_line) else {
                continue;
            };

            // Skip trivial self-recursion (fn foo calls foo directly).
            if sym.name == callee {
                continue;
            }

            let key = format!("{}::{}", file, sym.name);
            let edges = result.entry(key).or_default();

            // Keep only the first call site per (caller, callee) pair.
            if !edges.iter().any(|e| e.callee == callee) {
                edges.push(CallEdge { callee, call_line });
            }
        }

        result
    }
}

// ── CST traversal ─────────────────────────────────────────────────────────────

/// Walk the entire CST iteratively and collect (line, callee_name) for every
/// call site.  Uses an explicit stack to avoid recursion-depth issues on
/// deeply nested expressions.
fn collect_calls(root: Node, src: &[u8], calls: &mut Vec<(usize, String)>) {
    let mut stack: Vec<Node> = vec![root];

    while let Some(node) = stack.pop() {
        match node.kind() {
            // Regular call: foo(), foo::bar(), obj.method() via field_expression
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    if let Some(name) = callee_name(func, src) {
                        calls.push((node.start_position().row + 1, name));
                    }
                }
            }
            // Method call syntax (present in some grammar versions): obj.method()
            "method_call_expression" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(src) {
                        if !name.is_empty() {
                            calls.push((node.start_position().row + 1, name.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }

        // Push children in reverse so left-to-right order is preserved.
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

/// Resolve the callee name from a `function` expression node.
///
/// Handles the four shapes that can appear as the `function` child of a
/// `call_expression`:
/// - `identifier`        — `foo()`
/// - `scoped_identifier` — `foo::bar()` or `crate::mod::func()`
/// - `field_expression`  — `self.method()` or `state.run()`
/// - `generic_function`  — `collect::<Vec<_>>()` — recurse into its function
fn callee_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(src).ok().map(|s| s.to_string()),

        // Take the last path segment: `crate::tools::pie_tool::execute` → `execute`
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string()),

        // `self.run_tui` → `run_tui`
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(src).ok())
            .map(|s| s.to_string()),

        // `foo::<T>()` — strip the type arguments and recurse
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| callee_name(n, src)),

        _ => None,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Find the innermost symbol in `file_syms` whose line range contains
/// `call_line`.  "Innermost" = highest start line ≤ call_line.
///
/// `file_syms` must already be filtered to the same file and sorted by line.
fn find_containing_symbol<'a>(file_syms: &[&'a Symbol], call_line: usize) -> Option<&'a Symbol> {
    file_syms
        .iter()
        .filter(|s| s.line <= call_line && call_line <= s.end_line)
        .max_by_key(|s| s.line)
        .copied()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SymbolKind, compute_end_lines};

    fn make_sym(name: &str, file: &str, line: usize, end_line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            file: file.to_string(),
            line,
            end_line,
            kind: SymbolKind::Function,
            signature: None,
        }
    }

    fn known(names: &[&str]) -> HashMap<String, Vec<String>> {
        names
            .iter()
            .map(|n| (n.to_string(), vec!["src/fake.rs".to_string()]))
            .collect()
    }

    #[test]
    fn test_plain_call() {
        let src = r#"
fn caller() {
    dispatch_tool();
}
fn dispatch_tool() {}
"#;
        let mut symbols = vec![
            make_sym("caller", "src/a.rs", 2, 4),
            make_sym("dispatch_tool", "src/a.rs", 5, 5),
        ];
        let file_lines: HashMap<String, usize> =
            [("src/a.rs".to_string(), 6)].into_iter().collect();
        compute_end_lines(&mut symbols, &file_lines);

        let known = known(&["dispatch_tool"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::caller";
        assert!(edges.contains_key(key), "expected edge from caller");
        assert_eq!(edges[key][0].callee, "dispatch_tool");
    }

    #[test]
    fn test_scoped_call() {
        let src = r#"
fn caller() {
    crate::tools::execute(&args);
}
"#;
        let symbols = vec![make_sym("caller", "src/a.rs", 2, 4)];
        let known = known(&["execute"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::caller";
        assert!(edges.contains_key(key));
        assert_eq!(edges[key][0].callee, "execute");
    }

    #[test]
    fn test_method_call() {
        let src = r#"
fn caller(state: &mut AppState) {
    state.run_tui();
}
"#;
        let symbols = vec![make_sym("caller", "src/a.rs", 2, 4)];
        let known = known(&["run_tui"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::caller";
        assert!(edges.contains_key(key));
        assert_eq!(edges[key][0].callee, "run_tui");
    }

    #[test]
    fn test_external_calls_filtered() {
        let src = r#"
fn caller() {
    println!("hi");
    some_external_lib();
    project_fn();
}
"#;
        let symbols = vec![make_sym("caller", "src/a.rs", 2, 6)];
        // Only project_fn is known
        let known = known(&["project_fn"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::caller";
        assert!(edges.contains_key(key));
        assert_eq!(edges[key].len(), 1);
        assert_eq!(edges[key][0].callee, "project_fn");
    }

    #[test]
    fn test_dedup_multiple_calls_same_callee() {
        let src = r#"
fn caller() {
    helper();
    helper();
    helper();
}
"#;
        let symbols = vec![make_sym("caller", "src/a.rs", 2, 6)];
        let known = known(&["helper"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::caller";
        assert!(edges.contains_key(key));
        // Three calls, but only one edge (first occurrence)
        assert_eq!(edges[key].len(), 1);
        assert_eq!(edges[key][0].callee, "helper");
        assert_eq!(edges[key][0].call_line, 3);
    }

    #[test]
    fn test_self_recursion_excluded() {
        let src = r#"
fn recurse(n: usize) -> usize {
    if n == 0 { 0 } else { recurse(n - 1) }
}
"#;
        let symbols = vec![make_sym("recurse", "src/a.rs", 2, 4)];
        let known = known(&["recurse"]);
        let mut ext = CallExtractor::new().expect("tree-sitter init");
        let edges = ext.extract_file(src, "src/a.rs", &symbols, &known);

        // Self-recursion should be excluded
        assert!(edges.get("src/a.rs::recurse").map_or(true, |v| v.is_empty()));
    }
}
