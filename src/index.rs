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

/// A single indexed symbol.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone, PartialEq)]
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
    fn label(&self) -> &'static str {
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
            extract_symbols(&content, &rel, &mut index.symbols);
        }

        // Sort by file, then line
        index.symbols.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));

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

    /// Resolve a list of names/paths to a deduplicated list of real file paths.
    /// - If entry looks like a path (contains `/` or `.`), keep as-is
    /// - If entry matches a symbol name, substitute the file(s) it's defined in
    /// - Entries that don't match anything are kept (model may be right about new files)
    pub fn resolve_files(&self, entries: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for entry in entries {
            if entry.contains('/') || entry.contains('.') {
                // Looks like a path — keep it
                if !out.contains(entry) {
                    out.push(entry.clone());
                }
            } else if let Some(files) = self.by_name.get(entry.as_str()) {
                // Symbol name — substitute file(s)
                for f in files {
                    if !out.contains(f) {
                        out.push(f.clone());
                    }
                }
            } else {
                // Unknown — keep as-is
                if !out.contains(entry) {
                    out.push(entry.clone());
                }
            }
        }
        out
    }

    /// Produce a compact text representation for injection into a model prompt.
    /// Groups symbols by file, capped to avoid bloating the planning context.
    /// Format:
    ///   src/auth.rs: fn validate_token, struct AuthError, fn verify_claims
    ///   src/handler.rs: fn handle_request, fn handle_error
    pub fn to_prompt_section(&self, max_lines: usize) -> Option<String> {
        if self.symbols.is_empty() {
            return None;
        }

        // Group by file
        let mut by_file: Vec<(String, Vec<String>)> = Vec::new();
        for sym in &self.symbols {
            if let Some(last) = by_file.last_mut() {
                if last.0 == sym.file {
                    last.1.push(format!("{} {}", sym.kind.label(), sym.name));
                    continue;
                }
            }
            by_file.push((sym.file.clone(), vec![format!("{} {}", sym.kind.label(), sym.name)]));
        }

        let mut lines: Vec<String> = Vec::new();
        for (file, syms) in &by_file {
            if lines.len() >= max_lines {
                break;
            }
            // Truncate symbol list if very long
            let sym_list = if syms.len() > 12 {
                format!("{}, … ({} total)", syms[..12].join(", "), syms.len())
            } else {
                syms.join(", ")
            };
            lines.push(format!("  {file}: {sym_list}"));
        }

        if lines.is_empty() {
            return None;
        }

        let truncation_note = if by_file.len() > max_lines {
            format!("\n  … and {} more files", by_file.len() - max_lines)
        } else {
            String::new()
        };

        Some(format!(
            "# Project symbol index\nUse these symbol names and paths in the \"files\" field of each step:\n\n{}{}\n",
            lines.join("\n"),
            truncation_note
        ))
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

fn extract_symbols(content: &str, file: &str, out: &mut Vec<Symbol>) {
    let ext = Path::new(file)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();

    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(sym) = extract_symbol_from_line(trimmed, &ext, line_no + 1, file) {
            out.push(sym);
        }
    }
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
        kind,
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
    fn test_resolve_files() {
        let mut index = SymbolIndex::default();
        index.symbols.push(Symbol {
            name: "validate_token".to_string(),
            file: "src/auth.rs".to_string(),
            line: 10,
            kind: SymbolKind::Function,
        });
        index.by_name.insert(
            "validate_token".to_string(),
            vec!["src/auth.rs".to_string()],
        );

        // Path-like entry — kept as-is
        let result = index.resolve_files(&["src/main.rs".to_string()]);
        assert_eq!(result, vec!["src/main.rs"]);

        // Symbol name — resolved to file
        let result = index.resolve_files(&["validate_token".to_string()]);
        assert_eq!(result, vec!["src/auth.rs"]);

        // Mixed
        let result = index.resolve_files(&[
            "src/main.rs".to_string(),
            "validate_token".to_string(),
        ]);
        assert_eq!(result, vec!["src/main.rs", "src/auth.rs"]);
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
