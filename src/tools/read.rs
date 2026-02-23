use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

/// Default max lines returned without an explicit range.
const DEFAULT_MAX_LINES: usize = 500;
/// How many lines of preamble (imports/use declarations) to always include.
const PREAMBLE_LINES: usize = 80;
/// How many tail lines to always include (must cover the closing of the last function).
const TAIL_LINES: usize = 120;

// ── Line hashing ─────────────────────────────────────────────────────────────

/// FNV-1a 32-bit hash of a line's content, encoded as 4 lower-case base-36 chars.
/// Used by edit_file anchor validation — lets the model confirm a line hasn't
/// shifted or changed since it was last read. No external deps needed.
pub fn line_hash(content: &str) -> String {
    let mut h: u32 = 2166136261;
    for b in content.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = [b'0'; 4];
    let mut v = (h as usize) % (36_usize.pow(4)); // 36^4 = 1_679_616
    for i in (0..4).rev() {
        out[i] = DIGITS[v % 36];
        v /= 36;
    }
    String::from_utf8(out.to_vec()).unwrap()
}

/// Format one numbered+hashed content line.  `  42 [a3f2] | <content>`
pub fn format_line(line_num: usize, content: &str) -> String {
    format!("{:4} [{}] | {}\n", line_num, line_hash(content), content)
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn definition() -> Value {
    serde_json::json!({
        "name": "read_file",
        "description": "Read a file with line numbers and content hashes. Returns up to 150 lines by default; pass line_range for a specific section; pass symbols=true to get a function/class index. Each line is prefixed `N [hash] | content` — the 4-char hash in brackets is the anchor for edit_file.",
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

/// Format file content for injection into a model context (plan step pre-loading).
/// Small files: full content with line numbers + hashes.
/// Large files: preamble + symbol index + tail.
pub fn format_for_context(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total <= DEFAULT_MAX_LINES {
        format_full(&lines, path)
    } else {
        let preamble_end = preamble_end_line(&lines).min(total);
        let tail_start = total.saturating_sub(TAIL_LINES).max(preamble_end);
        let omitted = tail_start.saturating_sub(preamble_end);

        let mut out = String::new();
        out.push_str(&format!(
            "[{path} — {total} lines total. Preamble (1–{preamble_end}), symbol index, then tail ({start}–{total}). \
             Use line_range=[start,end] to read any section.]\n\n",
            start = tail_start + 1,
        ));
        for (i, line) in lines[..preamble_end].iter().enumerate() {
            out.push_str(&format_line(i + 1, line));
        }
        if omitted > 0 {
            out.push_str(&format!("\n     ··· {omitted} lines omitted — symbol index for navigation ···\n\n"));
            let symbols = collect_symbols(&lines[preamble_end..tail_start], preamble_end);
            if symbols.is_empty() {
                out.push_str("     (no top-level symbols detected in omitted section)\n");
            } else {
                for (line_no, hash, label) in &symbols {
                    out.push_str(&format!("{line_no:4} [{hash}] | {label}\n"));
                }
            }
            out.push('\n');
        }
        for (i, line) in lines[tail_start..].iter().enumerate() {
            out.push_str(&format_line(tail_start + i + 1, line));
        }
        out
    }
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"]
        .as_str()
        .context("read_file: missing 'path'")?;

    let content = fs::read_to_string(path)
        .with_context(|| format!("read_file: cannot read '{path}'"))?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Explicit range takes priority — if line_range is given, always return content not symbols
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

    // No range — smart excerpt: full file if small enough, else preamble + symbol index + tail
    if total <= DEFAULT_MAX_LINES {
        // Small file: always return full content with hashes, even if symbols=true.
        // The model needs hashes to make edits — symbol-only output is useless for small files.
        return Ok(format_full(&lines, path));
    }

    // Symbol index mode (large files only) — navigation aid, not editable content
    if args["symbols"].as_bool().unwrap_or(false) {
        return Ok(build_symbol_index(&lines, path, total));
    }

    // Large file: preamble + inline symbol index + tail.
    let preamble_end = preamble_end_line(&lines).min(total);
    let tail_start = total.saturating_sub(TAIL_LINES).max(preamble_end);
    let omitted = tail_start.saturating_sub(preamble_end);

    let mut out = String::new();
    out.push_str(&format!(
        "[{path} — {total} lines total. Preamble (1–{preamble_end}), symbol index, then tail ({start}–{total}). \
         Use line_range=[start,end] to read any section.]\n\n",
        start = tail_start + 1,
    ));

    // Section 1: preamble
    for (i, line) in lines[..preamble_end].iter().enumerate() {
        out.push_str(&format_line(i + 1, line));
    }

    // Section 2: inline symbol index for the omitted middle
    if omitted > 0 {
        out.push_str(&format!("\n     ··· {omitted} lines omitted — symbol index for navigation ···\n\n"));
        let symbols = collect_symbols(&lines[preamble_end..tail_start], preamble_end);
        if symbols.is_empty() {
            out.push_str("     (no top-level symbols detected in omitted section)\n");
        } else {
            for (line_no, hash, label) in &symbols {
                out.push_str(&format!("{line_no:4} [{hash}] | {label}\n"));
            }
        }
        out.push('\n');
    }

    // Section 3: tail (always shows end-of-file)
    for (i, line) in lines[tail_start..].iter().enumerate() {
        out.push_str(&format_line(tail_start + i + 1, line));
    }

    Ok(out)
}

// ── Preamble detection ────────────────────────────────────────────────────────

fn preamble_end_line(lines: &[&str]) -> usize {
    let cap = PREAMBLE_LINES * 2;
    let mut last_import = 0usize;
    for (i, line) in lines.iter().take(cap).enumerate() {
        let t = line.trim();
        if t.starts_with("use ")
            || t.starts_with("import ")
            || t.starts_with("mod ")
            || t.starts_with("from ")
            || t.starts_with("#include")
            || t.starts_with("require(")
            || t.is_empty()
            || t.starts_with("//")
            || t.starts_with('#')
        {
            last_import = i + 1;
        } else {
            break;
        }
    }
    last_import.max(PREAMBLE_LINES).min(lines.len())
}

// ── Symbol index ──────────────────────────────────────────────────────────────

/// Collect symbol definitions from a slice of lines, returning (absolute_line_no, hash, label).
/// `offset` is the 0-based index of `lines[0]` in the full file.
/// The hash is computed from the *actual* file line (not the condensed label) so it
/// matches what edit_file expects for anchor validation.
pub fn collect_symbols(lines: &[&str], offset: usize) -> Vec<(usize, String, String)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            classify_symbol(line.trim()).map(|label| {
                let hash = line_hash(line);
                (offset + i + 1, hash, label)
            })
        })
        .collect()
}

fn build_symbol_index(lines: &[&str], path: &str, total: usize) -> String {
    let symbols = collect_symbols(lines, 0);

    if symbols.is_empty() {
        return format!(
            "[{path} — {total} lines. No top-level symbols found. Use line_range to read sections.]\n"
        );
    }

    let mut out = format!("[{path} — {total} lines. Symbol index (hashes are valid for edit_file anchor):]\n\n");
    for (line_no, hash, label) in &symbols {
        out.push_str(&format!("{line_no:4} [{hash}] | {label}\n"));
    }
    out.push_str("\nUse line_range=[start,end] to read any section.\n");
    out
}

fn classify_symbol(line: &str) -> Option<String> {
    if line.is_empty() || line.starts_with("//") || line.starts_with('#')
        || line.starts_with('*') || line.starts_with("/*")
    {
        return None;
    }

    // Rust
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
                return Some(format!("{} {name}", prefix.trim_end()));
            }
        }
    }

    // TypeScript/JavaScript
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

    // Python
    for prefix in &["async def ", "def ", "class "] {
        if line.starts_with(prefix) {
            let rest = &line[prefix.len()..];
            let name = rest.split(|c: char| c == '(' || c == ':').next().unwrap_or(rest);
            if !name.is_empty() {
                return Some(format!("{prefix}{name}"));
            }
        }
    }

    // Go
    if line.starts_with("func ") {
        let rest = &line[5..];
        let name = rest.split(|c: char| c == '(' || c == ' ').next().unwrap_or(rest);
        if !name.is_empty() {
            return Some(format!("func {name}"));
        }
    }

    None
}

// ── Formatters ────────────────────────────────────────────────────────────────

fn format_full(lines: &[&str], path: &str) -> String {
    let mut out = format!("[{}]\n\n", path);
    for (i, line) in lines.iter().enumerate() {
        out.push_str(&format_line(i + 1, line));
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
        out.push_str(&format_line(start + i + 1, line));
    }
    out
}
