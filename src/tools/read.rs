use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

/// Default max lines returned without an explicit range.
const DEFAULT_MAX_LINES: usize = 150;
/// How many lines of preamble (imports/declarations) to always include.
const PREAMBLE_LINES: usize = 50;
/// How many tail lines to always include.
const TAIL_LINES: usize = 20;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "read_file",
        "description": "Read a file with line numbers. Returns up to 150 lines by default; pass line_range for a specific section; pass symbols=true to get a function/class index instead of content.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to read"
                },
                "line_range": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Optional [start, end] (1-indexed, inclusive)"
                },
                "symbols": {
                    "type": "boolean",
                    "description": "Return a symbol index (functions, classes, structs) instead of file content. Useful for navigating large files before requesting a specific line_range."
                }
            },
            "required": ["path"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"]
        .as_str()
        .context("read_file: missing 'path'")?;

    let content = fs::read_to_string(path)
        .with_context(|| format!("read_file: cannot read '{path}'"))?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Symbol index mode — return function/class/struct definitions with line numbers
    if args["symbols"].as_bool().unwrap_or(false) {
        return Ok(build_symbol_index(&lines, path, total));
    }

    // Explicit range requested
    if let Some(range) = args["line_range"].as_array() {
        let start = range
            .first()
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).saturating_sub(1))
            .unwrap_or(0)
            .min(total.saturating_sub(1));
        let end = range
            .get(1)
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(total))
            .unwrap_or(total);

        return Ok(format_excerpt(&lines, start, end, total, path));
    }

    // No range — smart excerpt: full file if small enough, else preamble + tail
    if total <= DEFAULT_MAX_LINES {
        return Ok(format_full(&lines, path));
    }

    // Large file: preamble (imports/declarations) + tail, with omission marker
    let preamble_end = PREAMBLE_LINES.min(total);
    let tail_start = total.saturating_sub(TAIL_LINES).max(preamble_end);

    let mut out = String::new();
    out.push_str(&format!(
        "[{path} — {total} lines total. Showing preamble (1-{preamble_end}) and tail ({}-{total}). Use symbols=true to find definitions, or line_range=[start,end] to read a section.]\n\n",
        tail_start + 1
    ));
    for (i, line) in lines[..preamble_end].iter().enumerate() {
        out.push_str(&format!("{:4} | {}\n", i + 1, line));
    }
    if tail_start > preamble_end {
        out.push_str(&format!("\n     ... ({} lines omitted) ...\n\n", tail_start - preamble_end));
    }
    for (i, line) in lines[tail_start..].iter().enumerate() {
        out.push_str(&format!("{:4} | {}\n", tail_start + i + 1, line));
    }

    Ok(out)
}

/// Scan the file for top-level symbol definitions and return them with line numbers.
/// Covers Rust, TypeScript/JavaScript, Python, Go, and C/C++ patterns.
fn build_symbol_index(lines: &[&str], path: &str, total: usize) -> String {
    // Patterns: (label, prefix to match after trimming)
    // We do simple prefix/contains matching — no regex dep needed.
    let mut symbols: Vec<(usize, String)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(label) = classify_symbol(trimmed) {
            symbols.push((i + 1, label));
        }
    }

    if symbols.is_empty() {
        return format!(
            "[{path} — {total} lines. No top-level symbols found. Use line_range to read sections.]\n"
        );
    }

    let mut out = format!("[{path} — {total} lines. Symbol index:]\n\n");
    for (line_no, label) in &symbols {
        out.push_str(&format!("{line_no:4} | {label}\n"));
    }
    out.push_str(&format!(
        "\nUse line_range=[start,end] to read any section.\n"
    ));
    out
}

/// Classify a trimmed line as a named symbol, returning a short label, or None.
fn classify_symbol(line: &str) -> Option<String> {
    // Skip blank lines and comment lines
    if line.is_empty() || line.starts_with("//") || line.starts_with('#')
        || line.starts_with('*') || line.starts_with("/*")
    {
        return None;
    }

    // Rust: fn, pub fn, async fn, pub async fn, struct, enum, impl, trait, mod, const, type
    for prefix in &["pub async fn ", "pub fn ", "async fn ", "fn ",
                    "pub struct ", "struct ",
                    "pub enum ", "enum ",
                    "impl ", "pub trait ", "trait ",
                    "pub mod ", "mod ",
                    "pub const ", "const ",
                    "pub type ", "type "] {
        if line.starts_with(prefix) {
            let rest = &line[prefix.len()..];
            let name = rest.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or(rest);
            if !name.is_empty() {
                return Some(format!("{}{}", prefix.trim_end(), format!(" {name}")));
            }
        }
    }

    // TypeScript/JavaScript: function, class, interface, type, const/let/var (arrow fns), export
    for prefix in &["export default function ", "export function ", "export class ",
                    "export interface ", "export type ", "export enum ",
                    "export const ", "export async function ",
                    "function ", "class ", "interface ", "async function "] {
        if line.starts_with(prefix) {
            let rest = &line[prefix.len()..];
            let name = rest.split(|c: char| c == '(' || c == '<' || c == ' ' || c == ':').next().unwrap_or(rest);
            if !name.is_empty() {
                return Some(format!("{}{name}", prefix.trim_end()));
            }
        }
    }

    // Python: def, class, async def
    for prefix in &["async def ", "def ", "class "] {
        if line.starts_with(prefix) {
            let rest = &line[prefix.len()..];
            let name = rest.split(|c: char| c == '(' || c == ':').next().unwrap_or(rest);
            if !name.is_empty() {
                return Some(format!("{prefix}{name}"));
            }
        }
    }

    // Go: func
    if line.starts_with("func ") {
        let rest = &line[5..];
        let name = rest.split(|c: char| c == '(' || c == ' ').next().unwrap_or(rest);
        if !name.is_empty() {
            return Some(format!("func {name}"));
        }
    }

    // C/C++: very rough — skip for now (too noisy without a real parser)

    None
}

fn format_full(lines: &[&str], path: &str) -> String {
    let mut out = format!("[{}]\n\n", path);
    for (i, line) in lines.iter().enumerate() {
        out.push_str(&format!("{:4} | {}\n", i + 1, line));
    }
    out
}

fn format_excerpt(lines: &[&str], start: usize, end: usize, total: usize, path: &str) -> String {
    let end = end.min(total);
    let mut out = format!(
        "[{path} — lines {}-{} of {}]\n\n",
        start + 1,
        end,
        total
    );
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:4} | {}\n", start + i + 1, line));
    }
    out
}
