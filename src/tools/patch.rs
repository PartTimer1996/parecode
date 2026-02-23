/// `patch_file` tool — apply a unified diff to a file.
///
/// More token-efficient than `edit_file` for multi-hunk changes. The model sends
/// only the changed lines instead of full old+new content for each hunk.
///
/// Parsing strategy:
///   1. Split patch string into hunks on `@@ ... @@` headers
///   2. For each hunk, separate context lines (no prefix) from `-` (remove) and `+` (add) lines
///   3. Locate the hunk in the file by matching context + removal lines (fuzzy: whitespace-tolerant)
///   4. Apply: replace matched region with context + addition lines
///
/// Error strategy: if any hunk fails to locate, return a clear error with the
/// region where we searched, so the model can correct the patch.
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::fs;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "patch_file",
        "description": "Apply a unified diff patch to a file. More token-efficient than edit_file for multi-hunk changes — send only the changed lines. Use edit_file for single-location changes; use patch_file when modifying multiple separate locations in the same file or making large structured changes.\n\nPatch format — standard unified diff:\n```\n@@ -15,4 +15,6 @@\n fn validate_token(token: &str) -> Result<Claims> {\n-    let claims = decode(token)?;\n+    let claims = decode(token)\n+        .map_err(|e| AuthError::Invalid(e.to_string()))?;\n     Ok(claims)\n }\n```\nRules:\n- Lines starting with ' ' are context (must match file exactly, used for anchoring)\n- Lines starting with '-' are removed\n- Lines starting with '+' are added\n- `@@` line numbers are hints only — actual location found by matching context lines\n- Omit the `--- a/` and `+++ b/` file headers; start directly with `@@`",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to patch"
                },
                "patch": {
                    "type": "string",
                    "description": "Unified diff patch string. Must contain at least one @@ hunk header."
                }
            },
            "required": ["path", "patch"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"].as_str().context("patch_file: missing 'path'")?;
    let patch = args["patch"].as_str().context("patch_file: missing 'patch'")?;

    let content = fs::read_to_string(path)
        .with_context(|| format!("patch_file: cannot read '{path}'"))?;

    let hunks = parse_hunks(patch)?;
    if hunks.is_empty() {
        return Err(anyhow!("patch_file: no @@ hunk headers found in patch"));
    }

    let mut current = content.clone();
    let mut hunks_applied = 0;

    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        match apply_hunk(&current, hunk) {
            Ok(new_content) => {
                current = new_content;
                hunks_applied += 1;
            }
            Err(e) => {
                return Err(anyhow!(
                    "patch_file: hunk {}/{} failed — {e}\n\
                     ({hunks_applied} of {} hunks applied before this failure)",
                    hunk_idx + 1,
                    hunks.len(),
                    hunks.len(),
                ));
            }
        }
    }

    fs::write(path, &current)
        .with_context(|| format!("patch_file: cannot write '{path}'"))?;

    // Find the approximate centre of the last applied hunk for context echo
    let last_hunk = &hunks[hunks_applied - 1];
    let anchor_line = find_hunk_line(&current, last_hunk).unwrap_or(1);
    let ctx = post_patch_context(path, &current, anchor_line);

    Ok(format!(
        "✓ Patched {path} ({hunks_applied}/{} hunks applied){ctx}",
        hunks.len()
    ))
}

// ── Hunk data structure ────────────────────────────────────────────────────────

#[derive(Debug)]
struct Hunk {
    /// Lines that must be present before the change (context + removals interleaved).
    /// Each entry is (line_content, is_removal).
    before: Vec<(String, bool)>,
    /// Lines to insert in place of removals (additions only).
    additions: Vec<String>,
    /// Hint from the @@ header (0-based), used as search start offset.
    line_hint: usize,
}

// ── Parser ─────────────────────────────────────────────────────────────────────

fn parse_hunks(patch: &str) -> Result<Vec<Hunk>> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    // Strip optional --- / +++ file headers
    let lines: Vec<&str> = patch
        .lines()
        .filter(|l| !l.starts_with("--- ") && !l.starts_with("+++ "))
        .collect();

    for line in &lines {
        if line.starts_with("@@ ") {
            // Commit the previous hunk
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            // Parse hint line number from "@@ -N,n +M,m @@" — we only care about old start
            let hint = parse_hunk_start(line);
            current = Some(Hunk {
                before: Vec::new(),
                additions: Vec::new(),
                line_hint: hint.saturating_sub(1), // convert to 0-based
            });
        } else if let Some(ref mut h) = current {
            if let Some(rest) = line.strip_prefix('-') {
                h.before.push((rest.to_string(), true));
            } else if let Some(rest) = line.strip_prefix('+') {
                h.additions.push(rest.to_string());
            } else {
                // Context line — strip the leading space if present
                let ctx_line = line.strip_prefix(' ').unwrap_or(line);
                h.before.push((ctx_line.to_string(), false));
            }
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }

    Ok(hunks)
}

/// Extract the old-file start line from "@@ -N,n +M,m @@"
fn parse_hunk_start(header: &str) -> usize {
    // Find "-N" after "@@"
    header
        .split_whitespace()
        .find(|s| s.starts_with('-'))
        .and_then(|s| s[1..].split(',').next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(1)
}

// ── Hunk application ───────────────────────────────────────────────────────────

fn apply_hunk(content: &str, hunk: &Hunk) -> Result<String> {
    if hunk.before.is_empty() && hunk.additions.is_empty() {
        return Ok(content.to_string());
    }

    let file_lines: Vec<&str> = content.lines().collect();

    // Build the "needle" — the lines we need to find in the file
    // (context lines + removal lines, in order, without the +/- prefix)
    let needle: Vec<&str> = hunk.before.iter().map(|(l, _)| l.as_str()).collect();

    if needle.is_empty() {
        // Pure insertion — use hint to determine position
        return apply_pure_insertion(content, &file_lines, hunk);
    }

    // Try to find needle starting near the hint, then fall back to full scan
    let found = find_needle(&file_lines, &needle, hunk.line_hint);

    let (start, end) = found.ok_or_else(|| {
        let hint_ctx = context_around(&file_lines, hunk.line_hint, 6);
        anyhow!(
            "context lines not found in file.\n\
             Expected to find:\n{}\n\
             File content near hint (line {}):\n{}",
            needle.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"),
            hunk.line_hint + 1,
            hint_ctx
        )
    })?;

    // Build the replacement: keep context lines, drop removal lines, insert additions
    let mut replacement: Vec<String> = Vec::new();
    for (line_content, is_removal) in &hunk.before {
        if !is_removal {
            replacement.push(line_content.clone());
        }
    }
    // Additions go after the context/removals at the position of the first removal,
    // or at the end of the context block if no removals.
    // Re-interleave: for each context line keep it; replace removal runs with additions.
    replacement.clear();
    let mut addition_idx = 0;
    let mut i = 0;
    while i < hunk.before.len() {
        let (line_content, is_removal) = &hunk.before[i];
        if !is_removal {
            replacement.push(line_content.clone());
            i += 1;
        } else {
            // Consume the whole run of removals and emit the additions in their place
            while i < hunk.before.len() && hunk.before[i].1 {
                i += 1;
            }
            while addition_idx < hunk.additions.len() {
                replacement.push(hunk.additions[addition_idx].clone());
                addition_idx += 1;
            }
        }
    }
    // Any trailing additions not yet emitted (patch ends with + lines, no following context)
    while addition_idx < hunk.additions.len() {
        replacement.push(hunk.additions[addition_idx].clone());
        addition_idx += 1;
    }

    // Stitch together: lines before match + replacement + lines after match
    let mut out_lines: Vec<&str> = file_lines[..start].to_vec();
    let repl_refs: Vec<&str> = replacement.iter().map(|s| s.as_str()).collect();
    out_lines.extend_from_slice(&repl_refs);
    out_lines.extend_from_slice(&file_lines[end..]);

    let mut out = out_lines.join("\n");
    // Preserve trailing newline
    if content.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Pure-insertion hunk (no context, no removals — only additions).
/// Inserts at `line_hint` position.
fn apply_pure_insertion(content: &str, file_lines: &[&str], hunk: &Hunk) -> Result<String> {
    let insert_at = hunk.line_hint.min(file_lines.len());
    let mut out_lines: Vec<&str> = file_lines[..insert_at].to_vec();
    let repl_refs: Vec<&str> = hunk.additions.iter().map(|s| s.as_str()).collect();
    out_lines.extend_from_slice(&repl_refs);
    out_lines.extend_from_slice(&file_lines[insert_at..]);
    let mut out = out_lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Search for `needle` in `file_lines`. Tries near `hint` first, then full scan.
/// Returns `(start, end)` as exclusive line indices if found exactly once.
fn find_needle(file_lines: &[&str], needle: &[&str], hint: usize) -> Option<(usize, usize)> {
    let n = needle.len();
    if n == 0 || file_lines.len() < n {
        return None;
    }

    // Try exact match first, hint-biased
    let candidates = collect_matches(file_lines, needle, |a, b| a == b);
    if candidates.len() == 1 {
        let s = candidates[0];
        return Some((s, s + n));
    }

    // Fuzzy: whitespace-trimmed comparison
    let candidates = collect_matches(file_lines, needle, |a, b| a.trim() == b.trim());
    if candidates.len() == 1 {
        let s = candidates[0];
        return Some((s, s + n));
    }

    // Multiple candidates — pick the one closest to the hint
    if !candidates.is_empty() {
        let best = candidates
            .iter()
            .min_by_key(|&&s| (s as isize - hint as isize).unsigned_abs())
            .copied()?;
        return Some((best, best + n));
    }

    None
}

fn collect_matches<F>(file_lines: &[&str], needle: &[&str], eq: F) -> Vec<usize>
where
    F: Fn(&str, &str) -> bool,
{
    let n = needle.len();
    let mut out = Vec::new();
    'outer: for start in 0..=file_lines.len().saturating_sub(n) {
        for (i, &nl) in needle.iter().enumerate() {
            if !eq(file_lines[start + i], nl) {
                continue 'outer;
            }
        }
        out.push(start);
    }
    out
}

/// Find approximate line position of a hunk in the patched file (for context echo).
fn find_hunk_line(content: &str, hunk: &Hunk) -> Option<usize> {
    let file_lines: Vec<&str> = content.lines().collect();
    // Look for the first non-removal line of the hunk in the result
    let needle: Vec<&str> = hunk
        .before
        .iter()
        .filter(|(_, rem)| !rem)
        .map(|(l, _)| l.as_str())
        .take(3)
        .collect();
    if needle.is_empty() {
        return Some(hunk.line_hint + 1);
    }
    let candidates = collect_matches(&file_lines, &needle, |a, b| a.trim() == b.trim());
    candidates.first().map(|&s| s + 1)
}

fn context_around(lines: &[&str], centre: usize, radius: usize) -> String {
    let lo = centre.saturating_sub(radius);
    let hi = (centre + radius).min(lines.len());
    lines[lo..hi]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("  {:>4}: {l}\n", lo + i + 1))
        .collect()
}

// ── Post-patch context echo ────────────────────────────────────────────────────

fn post_patch_context(path: &str, content: &str, anchor_line: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    let centre = anchor_line.saturating_sub(1).min(total - 1);
    let lo = centre.saturating_sub(8);
    let hi = (centre + 8).min(total);

    let mut out = format!(
        "\n[{path} after patch — lines {}-{} of {total}]\n",
        lo + 1,
        hi
    );
    for (i, line) in lines[lo..hi].iter().enumerate() {
        out.push_str(&crate::tools::read::format_line(lo + i + 1, line));
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_start() {
        assert_eq!(parse_hunk_start("@@ -15,4 +15,6 @@"), 15);
        assert_eq!(parse_hunk_start("@@ -1 +1 @@"), 1);
        assert_eq!(parse_hunk_start("@@ -200,3 +201,5 @@ fn foo()"), 200);
    }

    #[test]
    fn test_simple_replacement() {
        let content = "fn foo() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let hunk = Hunk {
            before: vec![
                ("fn foo() {".to_string(), false),
                ("    let x = 1;".to_string(), true),
            ],
            additions: vec!["    let x = 42;".to_string()],
            line_hint: 0,
        };
        let result = apply_hunk(content, &hunk).unwrap();
        assert!(result.contains("let x = 42;"));
        assert!(!result.contains("let x = 1;"));
    }

    #[test]
    fn test_fuzzy_whitespace_match() {
        let content = "fn bar() {\n    let y = 2;  \n    return y;\n}\n";
        let hunk = Hunk {
            before: vec![
                ("    let y = 2;".to_string(), true), // no trailing spaces in needle
            ],
            additions: vec!["    let y = 99;".to_string()],
            line_hint: 0,
        };
        let result = apply_hunk(content, &hunk).unwrap();
        assert!(result.contains("let y = 99;"));
    }

    #[test]
    fn test_multi_hunk_parse() {
        let patch = "@@ -1,3 +1,3 @@\n fn a() {}\n-let x = 1;\n+let x = 2;\n \n@@ -10,2 +10,2 @@\n fn b() {}\n-let y = 3;\n+let y = 4;\n";
        let hunks = parse_hunks(patch).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].additions, vec!["let x = 2;"]);
        assert_eq!(hunks[1].additions, vec!["let y = 4;"]);
    }

    #[test]
    fn test_addition_only_at_end() {
        let content = "fn foo() {\n}\n";
        let hunk = Hunk {
            before: vec![("fn foo() {".to_string(), false), ("}".to_string(), false)],
            additions: vec!["// added".to_string()],
            line_hint: 0,
        };
        // Additions after context — should insert between the two context lines
        // (since no removals, additions are trailing)
        let result = apply_hunk(content, &hunk).unwrap();
        assert!(result.contains("// added"));
    }

    #[test]
    fn test_trailing_newline_preserved() {
        let content = "line1\nline2\n";
        let hunk = Hunk {
            before: vec![("line1".to_string(), true)],
            additions: vec!["line1_new".to_string()],
            line_hint: 0,
        };
        let result = apply_hunk(content, &hunk).unwrap();
        assert!(result.ends_with('\n'));
    }
}
