/// Project symbol index — maps top-level symbol names to the files that define them.
///
/// Used during plan generation to give the model an accurate file map instead of
/// having it guess paths. The model names symbols it needs; the scaffold resolves
/// those names to real file paths.
///
/// Design constraints:
/// - Zero model calls — pure regex/text scan
/// - Fast enough to run on every `/plan` invocation (< 100ms for typical projects)
/// - Language-agnostic: covers Rust, TypeScript/JS, Python, Go, C/C++
/// - Output is compact text suitable for injection into a model prompt
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single outgoing call edge from a symbol to a callee by name.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CallEdge {
    /// Name of the called function or method.
    pub callee: String,
    /// Line number (1-indexed) in the source file where this call occurs.
    /// When a callee is called multiple times, this is the first occurrence.
    pub call_line: usize,
}

/// A single indexed symbol.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Symbol {
    pub name: String,
    pub file: String,
    pub line: usize,
    /// Last line of this symbol's body (inclusive). Computed post-sort as the
    /// line before the next symbol in the same file, or the file's total line count.
    #[serde(default)]
    pub end_line: usize,
    pub kind: SymbolKind,
    /// Compact interface summary extracted at index time:
    /// - fn: "(param: Type, ...) -> RetType" (signature, truncated at 120 chars)
    /// - struct: "field: Type, field: Type, ..." (pub fields, truncated at 120 chars)
    /// - enum: "Variant, Variant, ..." (variant names, truncated)
    /// None for impl blocks, traits, constants, and other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Method,
    Constant,
    Other,
}

impl SymbolKind {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Struct   => "struct",
            Self::Enum     => "enum",
            Self::Trait    => "trait",
            Self::Impl     => "impl",
            Self::Class    => "class",
            Self::Method   => "method",
            Self::Constant => "const",
            Self::Other    => "def",
        }
    }
}

/// Complete project symbol index.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    /// All symbols found, sorted by file then line number
    pub symbols: Vec<Symbol>,
    /// name → list of files (a name may be defined in multiple files)
    pub by_name: HashMap<String, Vec<String>>,
    /// file path → line count
    pub file_lines: HashMap<String, usize>,
}

impl SymbolIndex {
    /// Build an index by walking the project from `root`.
    /// Ignores noise directories (target, node_modules, etc.).
    /// Caps at `max_files` to keep runtime bounded.
    pub fn build(root: &Path, max_files: usize) -> Self {
        const IGNORED: &[&str] = &[
            "target", "node_modules", ".git", ".next", "dist", "build",
            "__pycache__", ".venv", "venv", ".cache", "coverage",
        ];
        const EXTENSIONS: &[&str] = &[
            "rs", "ts", "tsx", "js", "jsx", "py", "go", "c", "cpp", "h", "hpp",
        ];

        let mut index = SymbolIndex::default();
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(root, IGNORED, EXTENSIONS, &mut files, max_files);

        for path in &files {
            let Ok(content) = std::fs::read_to_string(path) else { continue };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let line_count = content.lines().count();
            index.file_lines.insert(rel.clone(), line_count);
            extract_symbols(&content, &rel, &mut index.symbols);
        }

        // Sort by file, then line
        index.symbols.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));

        // Compute end lines: next symbol's start - 1, or file's total line count
        compute_end_lines(&mut index.symbols, &index.file_lines);

        // Build name → files map
        for sym in &index.symbols {
            index
                .by_name
                .entry(sym.name.clone())
                .or_default()
                .push(sym.file.clone());
        }

        // Deduplicate file lists
        for files in index.by_name.values_mut() {
            files.dedup();
        }

        index
    }

}

// ── File collection ────────────────────────────────────────────────────────────

fn collect_files(
    dir: &Path,
    ignored: &[&str],
    extensions: &[&str],
    out: &mut Vec<PathBuf>,
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
        if name_str.starts_with('.') {
            continue;
        }
        if ignored.contains(&name_str.as_ref()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, ignored, extensions, out, max);
        } else if let Some(ext) = path.extension() {
            if extensions.contains(&ext.to_string_lossy().as_ref()) {
                out.push(path);
            }
        }
    }
}

// ── Symbol extraction ──────────────────────────────────────────────────────────

pub(crate) fn extract_symbols(content: &str, file: &str, out: &mut Vec<Symbol>) {
    let ext = Path::new(file)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();

    let lines: Vec<&str> = content.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        // Only index top-level symbols — lines that start at column 0 (no indentation).
        // Indented lines are inside function/impl bodies and must not create false
        // symbol boundaries (which would truncate end_line for the enclosing symbol).
        let first = line.chars().next();
        let is_top_level = matches!(first, Some(c) if !c.is_whitespace());
        if !is_top_level {
            continue;
        }
        let trimmed = line.trim();
        if let Some(mut sym) = extract_symbol_from_line(trimmed, &ext, i + 1, file) {
            sym.signature = extract_signature(&sym.kind, &ext, &lines, i);
            out.push(sym);
        }
    }
}

/// Extract a compact interface summary for a symbol using lookahead from its definition line.
fn extract_signature(kind: &SymbolKind, ext: &str, lines: &[&str], start: usize) -> Option<String> {
    if ext != "rs" {
        return None; // Only Rust for now
    }
    match kind {
        SymbolKind::Function => extract_fn_signature(lines, start),
        SymbolKind::Struct   => extract_struct_fields(lines, start),
        SymbolKind::Enum     => extract_enum_variants(lines, start),
        _ => None,
    }
}

/// Extract `(params) -> RetType` from a fn definition (may span multiple lines).
fn extract_fn_signature(lines: &[&str], start: usize) -> Option<String> {
    // Collect lines from start until we hit the opening `{` or a `;`
    let mut sig = String::new();
    for line in lines.iter().skip(start) {
        let t = line.trim();
        sig.push(' ');
        sig.push_str(t);
        if t.ends_with('{') || t.ends_with(';') {
            break;
        }
        if sig.len() > 300 { break; }
    }
    // Extract the part between the first `(` and the `{`
    let paren_start = sig.find('(')?;
    let brace = sig.rfind(" {")?;
    let inner = sig[paren_start..brace].trim().to_string();
    // Trim to 120 chars at a clean boundary
    Some(truncate_at_word(&inner, 120))
}

/// Extract pub fields from a struct body: `field: Type, field: Type, ...`
fn extract_struct_fields(lines: &[&str], start: usize) -> Option<String> {
    // Skip past the opening `{` of the struct
    let mut in_body = false;
    let mut fields: Vec<String> = Vec::new();

    for line in lines.iter().skip(start) {
        let t = line.trim();
        if !in_body {
            if t.contains('{') { in_body = true; }
            continue;
        }
        // Closing brace at col 0 = end of struct
        if t == "}" { break; }
        // Only pub fields (skip doc comments, attributes, private fields)
        if t.starts_with("pub ") && t.contains(':') && !t.starts_with("pub fn") {
            let field = t.trim_start_matches("pub(crate) ")
                         .trim_start_matches("pub ");
            // Strip trailing comma and any inline comment
            let field = field.split("//").next().unwrap_or(field).trim().trim_end_matches(',');
            if !field.is_empty() {
                fields.push(field.to_string());
            }
        }
        if fields.len() >= 12 { break; }
    }
    if fields.is_empty() { return None; }
    Some(truncate_at_word(&fields.join(", "), 120))
}

/// Extract variant names from an enum body.
fn extract_enum_variants(lines: &[&str], start: usize) -> Option<String> {
    let mut in_body = false;
    let mut variants: Vec<&str> = Vec::new();

    for line in lines.iter().skip(start) {
        let t = line.trim();
        if !in_body {
            if t.contains('{') { in_body = true; }
            continue;
        }
        if t == "}" { break; }
        // Skip doc comments and attributes
        if t.starts_with("//") || t.starts_with('#') { continue; }
        // Variant name is the first identifier on the line
        let name = t.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("").trim();
        if !name.is_empty() && is_ident(name) {
            variants.push(name);
        }
        if variants.len() >= 12 { break; }
    }
    if variants.is_empty() { return None; }
    Some(truncate_at_word(&variants.join(", "), 120))
}

/// Truncate a string at a word boundary to fit within `max` chars.
fn truncate_at_word(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let cut = s[..max].rfind(|c: char| c == ',' || c == ' ').unwrap_or(max);
    format!("{}…", &s[..cut])
}

fn extract_symbol_from_line(line: &str, ext: &str, line_no: usize, file: &str) -> Option<Symbol> {
    let (kind, name) = match ext {
        "rs" => extract_rust(line)?,
        "ts" | "tsx" | "js" | "jsx" => extract_ts(line)?,
        "py" => extract_python(line)?,
        "go" => extract_go(line)?,
        "c" | "cpp" | "h" | "hpp" => extract_c(line)?,
        _ => return None,
    };

    Some(Symbol {
        name,
        file: file.to_string(),
        line: line_no,
        end_line: line_no, // placeholder — overwritten by compute_end_lines after sort
        kind,
        signature: None,  // filled by extract_symbols after kind is known
    })
}

// ── Rust ──────────────────────────────────────────────────────────────────────

fn extract_rust(line: &str) -> Option<(SymbolKind, String)> {
    // pub fn / fn / pub async fn / async fn
    if let Some(rest) = strip_rust_fn(line) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Function, name));
    }
    // pub struct / struct
    if let Some(rest) = strip_prefix_variants(line, &["pub struct ", "pub(crate) struct ", "struct "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Struct, name));
    }
    // pub enum / enum
    if let Some(rest) = strip_prefix_variants(line, &["pub enum ", "pub(crate) enum ", "enum "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Enum, name));
    }
    // pub trait / trait
    if let Some(rest) = strip_prefix_variants(line, &["pub trait ", "pub(crate) trait ", "trait "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Trait, name));
    }
    // impl Type / impl<T> Type / impl Trait for Type
    if let Some(rest) = line.strip_prefix("impl") {
        let rest = rest.trim_start();
        // Skip generic params: impl<T> Foo -> skip to after >
        let rest = if rest.starts_with('<') {
            match rest.find('>') {
                Some(i) => rest[i + 1..].trim(),
                None => return None,
            }
        } else {
            rest
        };
        // "Trait for Type" → take last word; plain "Type" → take first word
        let name = if rest.contains(" for ") {
            rest.split(" for ").nth(1).and_then(|s| ident_at_start(s))?
        } else {
            ident_at_start(rest)?
        };
        return Some((SymbolKind::Impl, name));
    }
    // pub const / const
    if let Some(rest) = strip_prefix_variants(line, &["pub const ", "const "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Constant, name));
    }
    None
}

fn strip_rust_fn(line: &str) -> Option<&str> {
    let prefixes = [
        "pub async fn ", "pub(crate) async fn ", "async fn ",
        "pub fn ", "pub(crate) fn ", "fn ",
    ];
    strip_prefix_variants(line, &prefixes)
}

// ── TypeScript / JavaScript ────────────────────────────────────────────────────

fn extract_ts(line: &str) -> Option<(SymbolKind, String)> {
    // export async function / async function / function
    if let Some(rest) = strip_prefix_variants(line, &[
        "export async function ", "export function ",
        "async function ", "function ",
    ]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Function, name));
    }
    // export default function
    if let Some(rest) = line.strip_prefix("export default function ") {
        let name = ident_at_start(rest).unwrap_or_else(|| "default".to_string());
        return Some((SymbolKind::Function, name));
    }
    // export class / class
    if let Some(rest) = strip_prefix_variants(line, &["export class ", "export abstract class ", "class "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Class, name));
    }
    // export interface / interface
    if let Some(rest) = strip_prefix_variants(line, &["export interface ", "interface "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Struct, name)); // treat as struct-like
    }
    // export type
    if let Some(rest) = strip_prefix_variants(line, &["export type "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Other, name));
    }
    // const/let/var arrow functions: `export const foo = (`  or `const foo = async (`
    if let Some(rest) = strip_prefix_variants(line, &[
        "export const ", "export let ", "const ", "let ",
    ]) {
        let name = ident_at_start(rest)?;
        // Only capture if it looks like a function (has => or = async)
        if line.contains("=>") || line.contains("= async") || line.contains("= function") {
            return Some((SymbolKind::Function, name));
        }
    }
    None
}

// ── Python ─────────────────────────────────────────────────────────────────────

fn extract_python(line: &str) -> Option<(SymbolKind, String)> {
    if let Some(rest) = strip_prefix_variants(line, &["async def ", "def "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Function, name));
    }
    if let Some(rest) = line.strip_prefix("class ") {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Class, name));
    }
    None
}

// ── Go ────────────────────────────────────────────────────────────────────────

fn extract_go(line: &str) -> Option<(SymbolKind, String)> {
    if let Some(rest) = line.strip_prefix("func ") {
        // func (r *Receiver) Method(...) — extract method name
        if rest.starts_with('(') {
            // Skip the receiver, get to the method name
            let after_paren = rest.find(')')?.checked_add(2)?;
            let name = ident_at_start(rest.get(after_paren..)?)?;
            return Some((SymbolKind::Method, name));
        }
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Function, name));
    }
    if let Some(rest) = strip_prefix_variants(line, &["type "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Struct, name));
    }
    None
}

// ── C/C++ ────────────────────────────────────────────────────────────────────

fn extract_c(line: &str) -> Option<(SymbolKind, String)> {
    if let Some(rest) = strip_prefix_variants(line, &["struct ", "typedef struct "]) {
        let name = ident_at_start(rest)?;
        return Some((SymbolKind::Struct, name));
    }
    // Simple heuristic: line ends with `)` or `) {` and has an identifier before `(`
    // This catches `int foo(` style definitions without full C parsing
    if line.contains('(') && !line.starts_with("//") && !line.starts_with(" ") {
        if let Some(paren_pos) = line.find('(') {
            let before = line[..paren_pos].trim();
            let name = before.split_whitespace().last()?.to_string();
            if is_ident(&name) {
                return Some((SymbolKind::Function, name));
            }
        }
    }
    None
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn strip_prefix_variants<'a>(s: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    for prefix in prefixes {
        if let Some(rest) = s.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// Extract an identifier at the start of a string (stops at whitespace, `(`, `<`, `:`, `{`)
fn ident_at_start(s: &str) -> Option<String> {
    let s = s.trim();
    let end = s
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let name = &s[..end];
    if is_ident(name) { Some(name.to_string()) } else { None }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false)
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Fill `end_line` for every symbol. Must be called after symbols are sorted by file+line.
/// End = next symbol's start line - 1 within the same file, or the file's total line count.
pub fn compute_end_lines(symbols: &mut Vec<Symbol>, file_lines: &HashMap<String, usize>) {
    let n = symbols.len();
    for i in 0..n {
        let file_total = *file_lines.get(&symbols[i].file).unwrap_or(&symbols[i].line);
        let end = if i + 1 < n && symbols[i + 1].file == symbols[i].file {
            symbols[i + 1].line.saturating_sub(1)
        } else {
            file_total
        };
        symbols[i].end_line = end.max(symbols[i].line);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_symbols() {
        let cases = vec![
            ("pub fn validate_token(", SymbolKind::Function, "validate_token"),
            ("pub async fn handle_request(", SymbolKind::Function, "handle_request"),
            ("fn internal(", SymbolKind::Function, "internal"),
            ("pub struct AuthError {", SymbolKind::Struct, "AuthError"),
            ("pub enum Status {", SymbolKind::Enum, "Status"),
            ("pub trait Authenticate {", SymbolKind::Trait, "Authenticate"),
            ("impl AuthService {", SymbolKind::Impl, "AuthService"),
            ("pub const MAX_RETRIES:", SymbolKind::Constant, "MAX_RETRIES"),
        ];
        for (line, expected_kind, expected_name) in cases {
            let result = extract_rust(line);
            assert!(result.is_some(), "Failed to extract from: {line}");
            let (kind, name) = result.unwrap();
            assert_eq!(kind, expected_kind, "Wrong kind for: {line}");
            assert_eq!(name, expected_name, "Wrong name for: {line}");
        }
    }

    #[test]
    fn test_ts_symbols() {
        let cases = vec![
            ("export function processUser(", SymbolKind::Function, "processUser"),
            ("export async function fetchData(", SymbolKind::Function, "fetchData"),
            ("export class UserService {", SymbolKind::Class, "UserService"),
            ("export interface UserProfile {", SymbolKind::Struct, "UserProfile"),
        ];
        for (line, expected_kind, expected_name) in cases {
            let result = extract_ts(line);
            assert!(result.is_some(), "Failed to extract from: {line}");
            let (kind, name) = result.unwrap();
            assert_eq!(kind, expected_kind, "Wrong kind for: {line}");
            assert_eq!(name, expected_name, "Wrong name for: {line}");
        }
    }

    #[test]
    fn test_python_symbols() {
        assert_eq!(
            extract_python("def process_request("),
            Some((SymbolKind::Function, "process_request".to_string()))
        );
        assert_eq!(
            extract_python("async def fetch_data("),
            Some((SymbolKind::Function, "fetch_data".to_string()))
        );
        assert_eq!(
            extract_python("class UserService:"),
            Some((SymbolKind::Class, "UserService".to_string()))
        );
    }

    #[test]
    fn test_ident_at_start() {
        assert_eq!(ident_at_start("foo(bar)"), Some("foo".to_string()));
        assert_eq!(ident_at_start("MyStruct {"), Some("MyStruct".to_string()));
        assert_eq!(ident_at_start("  leading"), Some("leading".to_string()));
        assert_eq!(ident_at_start("(not_ident"), None);
        assert_eq!(ident_at_start(""), None);
    }
}
