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

    /// Extract struct/enum-variant construction edges from one Rust source file.
    ///
    /// Returns `HashMap<"file::caller", Vec<CallEdge>>` where `CallEdge.callee` is:
    /// - `"UiEvent::TokenStats"` for scoped enum-variant constructions
    /// - `"AppState"` for plain indexed-struct literals
    ///
    /// Used to populate `ProjectGraph::construct_edges` so orient can show
    /// which functions *create* a type, complementing call edges which show
    /// which functions *call* other functions.
    pub fn extract_constructions(
        &mut self,
        content: &str,
        file: &str,
        symbols: &[Symbol],
        known_names: &HashMap<String, Vec<String>>,
    ) -> HashMap<String, Vec<CallEdge>> {
        let mut result: HashMap<String, Vec<CallEdge>> = HashMap::new();
        let tree = match self.parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => return result,
        };
        let src = content.as_bytes();
        let file_syms: Vec<&Symbol> = symbols.iter().filter(|s| s.file == file).collect();

        let mut raw: Vec<(usize, String)> = Vec::new();
        collect_struct_constructions(tree.root_node(), src, known_names, &mut raw);

        for (line, constructed) in raw {
            let Some(sym) = find_containing_symbol(&file_syms, line) else { continue };
            // Skip self-referential: impl AppState constructing AppState (or UiEvent::Foo)
            // happens when fn new() / Default::default() is inside impl but not indexed.
            let base = constructed.split("::").next().unwrap_or(&constructed);
            if sym.name == base {
                continue;
            }
            let key = format!("{}::{}", file, sym.name);
            let edges = result.entry(key).or_default();
            // Dedup: one entry per (caller, constructed) pair — first site wins.
            if !edges.iter().any(|e| e.callee == constructed) {
                edges.push(CallEdge { callee: constructed, call_line: line });
            }
        }
        result
    }

    /// Extract compact type signatures for all structs, enums, and traits in a file.
    /// Returns a map: symbol_name → compact definition string.
    /// Used to enrich Symbol.signature during indexing so find_symbol returns
    /// full field/variant lists without requiring a file read.
    pub fn extract_signatures(&mut self, content: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let tree = match self.parser.parse(content.as_bytes(), None) {
            Some(t) => t,
            None => return result,
        };
        let src = content.as_bytes();
        collect_type_signatures(tree.root_node(), src, &mut result);
        result
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

/// Walk the CST and collect struct-expression construction sites.
///
/// For scoped constructions like `UiEvent::TokenStats { ... }` we record the
/// full path string "UiEvent::TokenStats" — always included (unambiguous enum variant).
/// For plain struct literals like `AppState { ... }` we record the type name only
/// if it appears in `known_names` (i.e. it's an indexed project symbol).
/// For `Self { ... }` inside an impl block we resolve `Self` to the impl's type name,
/// catching common Rust constructor patterns like `impl Foo { fn new() -> Self { Self { } } }`.
fn collect_struct_constructions(
    root: Node,
    src: &[u8],
    known_names: &HashMap<String, Vec<String>>,
    out: &mut Vec<(usize, String)>,
) {
    // Pre-pass: collect impl block line ranges → base type name for `Self` resolution.
    // `impl<T> Foo<T> { ... }` → base = "Foo"
    let mut impl_ranges: Vec<(usize, usize, String)> = Vec::new();
    {
        let mut stack: Vec<Node> = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "impl_item" {
                if let Some(type_node) = node.child_by_field_name("type") {
                    if let Ok(raw) = type_node.utf8_text(src) {
                        let base = raw.split('<').next().unwrap_or(raw).trim().to_string();
                        if !base.is_empty() {
                            impl_ranges.push((
                                node.start_position().row + 1,
                                node.end_position().row + 1,
                                base,
                            ));
                        }
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    let mut stack: Vec<Node> = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "struct_expression" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let expr_line = node.start_position().row + 1;
                    let entry: Option<(usize, String)> = match name_node.kind() {
                        // Enum variant: UiEvent::TokenStats { ... } — always record, store full path
                        "scoped_type_identifier" => name_node
                            .utf8_text(src)
                            .ok()
                            .map(|s| (expr_line, s.to_string())),
                        "type_identifier" => name_node.utf8_text(src).ok().and_then(|s| {
                            if s == "Self" {
                                // Resolve Self → enclosing impl type name
                                impl_ranges.iter()
                                    .find(|(start, end, _)| expr_line >= *start && expr_line <= *end)
                                    .map(|(_, _, type_name)| (expr_line, type_name.clone()))
                            } else if known_names.contains_key(s) {
                                // Plain indexed struct literal
                                Some((expr_line, s.to_string()))
                            } else {
                                None
                            }
                        }),
                        _ => None,
                    };
                    if let Some(pair) = entry {
                        out.push(pair);
                    }
                }
            }
            // TypeName::new() / TypeName::default() / TypeName::build() — common Rust
            // constructor patterns that don't use struct literal syntax.
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    if func.kind() == "scoped_identifier" {
                        if let (Some(path_node), Some(name_node)) = (
                            func.child_by_field_name("path"),
                            func.child_by_field_name("name"),
                        ) {
                            if let (Ok(type_name), Ok(method)) = (
                                path_node.utf8_text(src),
                                name_node.utf8_text(src),
                            ) {
                                if matches!(method, "new" | "default" | "build")
                                    && known_names.contains_key(type_name)
                                {
                                    let call_line = node.start_position().row + 1;
                                    out.push((call_line, type_name.to_string()));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
}

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

// ── Type signature extraction ─────────────────────────────────────────────────

fn collect_type_signatures(root: Node, src: &[u8], out: &mut HashMap<String, String>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "struct_item" => {
                if let Some((name, sig)) = struct_sig(node, src) {
                    out.insert(name, sig);
                }
            }
            "enum_item" => {
                if let Some((name, sig)) = enum_sig(node, src) {
                    out.insert(name, sig);
                }
            }
            "trait_item" => {
                if let Some((name, sig)) = trait_sig(node, src) {
                    out.insert(name, sig);
                }
            }
            _ => {}
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                stack.push(child);
            }
        }
    }
}

fn find_child_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == kind {
                return Some(child);
            }
        }
    }
    None
}

/// Format a list of items as `{ item1, item2, ... }`, truncating at `max_chars`.
fn format_compact(items: &[String], max_chars: usize) -> String {
    if items.is_empty() {
        return "{}".to_string();
    }
    let full = format!("{{ {} }}", items.join(", "));
    if full.len() <= max_chars {
        return full;
    }
    // Build up as many items as fit, leaving room for the truncation note
    let mut parts: Vec<&str> = Vec::new();
    let mut len = 4usize; // "{ " + " }"
    for item in items {
        let add = if parts.is_empty() { item.len() } else { 2 + item.len() };
        if len + add + 18 > max_chars { break; }
        parts.push(item.as_str());
        len += add;
    }
    let remaining = items.len() - parts.len();
    format!("{{ {}, ... (+{remaining} more) }}", parts.join(", "))
}

fn struct_sig(node: Node, src: &[u8]) -> Option<(String, String)> {
    let name = node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.to_string())?;

    let body = find_child_kind(node, "field_declaration_list")?;

    let mut fields: Vec<String> = Vec::new();
    for i in 0..body.child_count() {
        let child = body.child(i)?;
        if child.kind() != "field_declaration" { continue; }
        let field_name = child.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("?");
        let field_type = child.child_by_field_name("type")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("?");
        fields.push(format!("{field_name}: {field_type}"));
    }

    if fields.is_empty() { return None; }
    // No truncation cap — all struct fields must be visible; truncated signatures
    // cause follow-up tool calls that cost orders of magnitude more than the tokens saved.
    Some((name, format_compact(&fields, usize::MAX)))
}

fn enum_sig(node: Node, src: &[u8]) -> Option<(String, String)> {
    let name = node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.to_string())?;

    let body = find_child_kind(node, "enum_variant_list")?;

    let mut variants: Vec<String> = Vec::new();
    for i in 0..body.child_count() {
        let child = body.child(i)?;
        if child.kind() != "enum_variant" { continue; }

        let vname = child.child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("?");

        if let Some(fields) = find_child_kind(child, "field_declaration_list") {
            // Struct-like variant: Foo { field: Type, ... }
            let field_strs: Vec<String> = (0..fields.child_count())
                .filter_map(|j| fields.child(j))
                .filter(|n| n.kind() == "field_declaration")
                .filter_map(|fd| {
                    let fname = fd.child_by_field_name("name")
                        .and_then(|n| n.utf8_text(src).ok())?;
                    let ftype = fd.child_by_field_name("type")
                        .and_then(|n| n.utf8_text(src).ok())?;
                    Some(format!("{fname}: {ftype}"))
                })
                .collect();
            if field_strs.is_empty() {
                variants.push(vname.to_string());
            } else {
                variants.push(format!("{vname} {{ {} }}", field_strs.join(", ")));
            }
        } else if let Some(tuple) = find_child_kind(child, "ordered_field_declaration_list") {
            // Tuple-like variant: Foo(T1, T2)
            let types: Vec<String> = (0..tuple.child_count())
                .filter_map(|j| tuple.child(j))
                .filter(|n| !matches!(n.kind(), "(" | ")" | "," | "visibility_modifier"))
                .filter_map(|n| n.utf8_text(src).ok().map(|s| s.to_string()))
                .collect();
            if types.is_empty() {
                variants.push(vname.to_string());
            } else {
                variants.push(format!("{vname}({})", types.join(", ")));
            }
        } else {
            variants.push(vname.to_string());
        }
    }

    if variants.is_empty() { return None; }
    Some((name, format_compact(&variants, usize::MAX)))
}

fn trait_sig(node: Node, src: &[u8]) -> Option<(String, String)> {
    let name = node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.to_string())?;

    let body = find_child_kind(node, "declaration_list")?;

    let mut methods: Vec<String> = Vec::new();
    for i in 0..body.child_count() {
        let child = body.child(i)?;
        if matches!(child.kind(), "function_signature_item" | "function_item") {
            if let Some(mname) = child.child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
            {
                methods.push(mname.to_string());
            }
        }
    }

    if methods.is_empty() { return None; }
    Some((name, format!("trait {{ {} }}", methods.join(", "))))
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

    #[test]
    fn test_extract_signatures_struct() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
struct Profile {
    model: String,
    cost_per_mtok_input: Option<f64>,
    cost_per_mtok_output: Option<f64>,
}
"#;
        let sigs = ext.extract_signatures(src);
        let sig = sigs.get("Profile").expect("Profile should be extracted");
        assert!(sig.contains("model: String"), "got: {sig}");
        assert!(sig.contains("cost_per_mtok_input: Option<f64>"), "got: {sig}");
        assert!(sig.contains("cost_per_mtok_output: Option<f64>"), "got: {sig}");
    }

    #[test]
    fn test_extract_signatures_enum() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
enum UiEvent {
    TokenStats { input_tokens: u32, output_tokens: u32 },
    AgentDone,
    Message(String),
}
"#;
        let sigs = ext.extract_signatures(src);
        let sig = sigs.get("UiEvent").expect("UiEvent should be extracted");
        assert!(sig.contains("TokenStats"), "got: {sig}");
        assert!(sig.contains("AgentDone"), "got: {sig}");
        assert!(sig.contains("Message"), "got: {sig}");
    }

    #[test]
    fn test_extract_signatures_trait() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
trait Execute {
    fn execute(&self) -> String;
    fn definition(&self) -> Value;
}
"#;
        let sigs = ext.extract_signatures(src);
        let sig = sigs.get("Execute").expect("Execute should be extracted");
        assert!(sig.contains("execute"), "got: {sig}");
        assert!(sig.contains("definition"), "got: {sig}");
    }

    #[test]
    fn test_extract_constructions_scoped_variant() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
fn run_tui() {
    let _ = ui_tx.send(UiEvent::TokenStats { input: 1, output: 2 });
    let _ = ui_tx.send(UiEvent::AgentDone);
}
"#;
        let symbols = vec![make_sym("run_tui", "src/a.rs", 2, 6)];
        let known = known(&["run_tui"]); // known_names doesn't need enum variants
        let edges = ext.extract_constructions(src, "src/a.rs", &symbols, &known);

        let key = "src/a.rs::run_tui";
        assert!(edges.contains_key(key), "expected construction edges from run_tui");
        let callees: Vec<&str> = edges[key].iter().map(|e| e.callee.as_str()).collect();
        assert!(callees.contains(&"UiEvent::TokenStats"), "expected TokenStats: {:?}", callees);
    }

    #[test]
    fn test_extract_constructions_plain_struct() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
fn make_state() -> AppState {
    AppState { name: "x".to_string() }
}
"#;
        let symbols = vec![make_sym("make_state", "src/a.rs", 2, 4)];
        let mut known_names = known(&["make_state"]);
        known_names.insert("AppState".to_string(), vec!["src/app.rs".to_string()]);
        let edges = ext.extract_constructions(src, "src/a.rs", &symbols, &known_names);

        let key = "src/a.rs::make_state";
        assert!(edges.contains_key(key), "expected construction edge for AppState");
        assert!(edges[key].iter().any(|e| e.callee == "AppState"));
    }

    #[test]
    fn test_extract_constructions_self_resolution() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
struct ResolvedConfig { endpoint: String }
impl ResolvedConfig {
    pub fn resolve() -> Self {
        Self { endpoint: "http://localhost".to_string() }
    }
}
"#;
        let symbols = vec![
            make_sym("ResolvedConfig", "src/config.rs", 2, 2),
            make_sym("resolve", "src/config.rs", 4, 6),
        ];
        let mut known_names = known(&["resolve"]);
        known_names.insert("ResolvedConfig".to_string(), vec!["src/config.rs".to_string()]);
        let edges = ext.extract_constructions(src, "src/config.rs", &symbols, &known_names);

        let key = "src/config.rs::resolve";
        assert!(edges.contains_key(key), "expected construction edge from resolve: {:?}", edges.keys().collect::<Vec<_>>());
        assert!(
            edges[key].iter().any(|e| e.callee == "ResolvedConfig"),
            "expected callee=ResolvedConfig, got: {:?}", edges.get(key)
        );
    }

    #[test]
    fn test_extract_constructions_dedup() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
fn sender() {
    send(UiEvent::TokenStats { input: 1, output: 1 });
    send(UiEvent::TokenStats { input: 2, output: 2 });
}
"#;
        let symbols = vec![make_sym("sender", "src/a.rs", 2, 6)];
        let known = known(&["sender"]);
        let edges = ext.extract_constructions(src, "src/a.rs", &symbols, &known);
        let key = "src/a.rs::sender";
        // Two constructions of the same variant — should be deduplicated to one edge
        let count = edges.get(key).map_or(0, |v| v.len());
        assert_eq!(count, 1, "expected dedup: {:?}", edges.get(key));
    }

    #[test]
    fn test_extract_constructions_new_method() {
        let mut ext = CallExtractor::new().unwrap();
        let src = r#"
fn setup(cfg: Config) -> AppState {
    AppState::new(cfg)
}
fn also_default() -> AppState {
    AppState::default()
}
"#;
        let symbols = vec![
            make_sym("setup", "src/a.rs", 2, 4),
            make_sym("also_default", "src/a.rs", 5, 7),
        ];
        let known = known(&["setup", "also_default", "AppState"]);
        let edges = ext.extract_constructions(src, "src/a.rs", &symbols, &known);
        let setup_edges = edges.get("src/a.rs::setup").expect("setup should have edges");
        assert!(setup_edges.iter().any(|e| e.callee == "AppState"), "::new() should be tracked");
        let def_edges = edges.get("src/a.rs::also_default").expect("also_default should have edges");
        assert!(def_edges.iter().any(|e| e.callee == "AppState"), "::default() should be tracked");
    }

    #[test]
    fn test_format_compact_truncation() {
        let items: Vec<String> = (0..20).map(|i| format!("field_{i}: SomeLongTypeName")).collect();
        let result = format_compact(&items, 200);
        assert!(result.len() <= 210, "should be near max: {}", result.len());
        assert!(result.contains("more"), "should note truncation: {result}");
    }
}
