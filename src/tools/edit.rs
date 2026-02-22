use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "edit_file",
        "description": "Edit a file. Two modes: (1) replace old_str with new_str — old_str must be unique in the file; (2) pass append=true with new_str to add content at the end of the file. On success, returns the file content around the edit site with fresh line numbers and hashes — use these for any follow-up edits without re-reading.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "old_str": {
                    "type": "string",
                    "description": "Exact string to find and replace. Must appear exactly once in the file — include enough surrounding context (function signature, preceding line, etc.) to make it unique. Omit when using append=true."
                },
                "new_str": {
                    "type": "string",
                    "description": "Replacement string (for old_str mode), or content to append (for append mode)"
                },
                "anchor": {
                    "type": "string",
                    "description": "The 4-char hash from the read_file line prefix. From '  42 [a3f2] | fn foo', the anchor is 'a3f2' (just the 4 chars inside the brackets). Do NOT include the line number or brackets."
                },
                "append": {
                    "type": "boolean",
                    "description": "If true, appends new_str to the end of the file. Use only for adding content that belongs at the top level and does not yet exist. If the target block already exists, use old_str to insert inside it instead — appending would place code outside the closing brace."
                }
            },
            "required": ["path", "new_str"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"].as_str().context("edit_file: missing 'path'")?;
    let new_str = args["new_str"]
        .as_str()
        .context("edit_file: missing 'new_str'")?;

    // ── Append mode ───────────────────────────────────────────────────────────
    if args["append"].as_bool().unwrap_or(false) {
        let mut content = fs::read_to_string(path)
            .with_context(|| format!("edit_file: cannot read '{path}'"))?;

        // Ensure file ends with a blank line so appended content starts cleanly
        if !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.ends_with("\n\n") {
            content.push('\n');
        }
        content.push_str(new_str);
        // Ensure file ends with newline after append
        if !content.ends_with('\n') {
            content.push('\n');
        }
        let append_start_line = content.lines().count() - new_str.lines().count() + 1;
        fs::write(path, &content)
            .with_context(|| format!("edit_file: cannot write '{path}'"))?;
        let added = new_str.lines().count();
        let ctx = post_edit_context(path, append_start_line);
        return Ok(format!("✓ Appended {added} lines to {path}{ctx}"));
    }

    // ── Replace mode ──────────────────────────────────────────────────────────
    let old_str = args["old_str"]
        .as_str()
        .context("edit_file: missing 'old_str' (required unless append=true)")?;

    // Guard: old_str too short to be reliably unique
    let old_str_trimmed_len = old_str.trim().len();
    if old_str_trimmed_len < 8 {
        return Err(anyhow::anyhow!(
            "edit_file: old_str is too short ({old_str_trimmed_len} chars after trimming). \
             Short strings like bare braces or keywords are almost always ambiguous. \
             Include at least one full line of surrounding context."
        ));
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("edit_file: cannot read '{path}'"))?;

    // Anchor check — verify the first line of old_str still has the expected hash.
    // This catches stale-line edits where the file changed since it was read.
    if let Some(anchor_raw) = args["anchor"].as_str() {
        // Normalise anchor to just the 4-char hash:
        //   "[a3f2]"    → "a3f2"  (model copied brackets from new format)
        //   "42#a3f2"   → "a3f2"  (old N#hash format)
        //   "a3f2"      → "a3f2"  (clean)
        let anchor: &str = &if anchor_raw.starts_with('[') && anchor_raw.ends_with(']') {
            anchor_raw[1..anchor_raw.len() - 1].to_string()
        } else if let Some(pos) = anchor_raw.rfind('#') {
            anchor_raw[pos + 1..].to_string()
        } else {
            anchor_raw.to_string()
        };
        let first_line = old_str.lines().next().unwrap_or("");
        let actual_hash = crate::tools::read::line_hash(first_line);
        if actual_hash != anchor {
            // Find where this first_line actually appears in the file for a useful hint
            let line_info = content.lines().enumerate()
                .find(|(_, l)| *l == first_line)
                .map(|(i, _)| format!(" (found at line {} with different hash)", i + 1))
                .unwrap_or_else(|| " (line not found in current file — content may have changed)".to_string());
            // Return the current content around where the model seems to be looking
            let hint_lines: Vec<String> = content.lines().enumerate()
                .filter(|(_, l)| l.trim() == first_line.trim())
                .flat_map(|(i, _)| {
                    let lo = i.saturating_sub(3);
                    let hi = (i + 4).min(content.lines().count());
                    content.lines().enumerate()
                        .skip(lo).take(hi - lo)
                        .map(|(j, l)| crate::tools::read::format_line(j + 1, l))
                        .collect::<Vec<_>>()
                })
                .take(12)
                .collect();
            let hint = if hint_lines.is_empty() {
                "Re-read the file to get current hashes.".to_string()
            } else {
                format!("Current content near that line:\n{}", hint_lines.join(""))
            };
            return Err(anyhow::anyhow!(
                "edit_file: anchor mismatch for '{path}' — expected hash '{anchor}' but got '{actual_hash}'{line_info}.\
                \n{hint}"
            ));
        }
    }

    // 1. Exact match — must be unique
    let exact_count = content.matches(old_str).count();
    if exact_count == 1 {
        let edit_byte = content.find(old_str).unwrap_or(0);
        let anchor_line = content[..edit_byte].lines().count() + 1;
        let new_content = content.replacen(old_str, new_str, 1);
        fs::write(path, &new_content)
            .with_context(|| format!("edit_file: cannot write '{path}'"))?;
        let ctx = post_edit_context(path, anchor_line);
        return Ok(format!("✓ Edited {path} (1 replacement){ctx}"));
    }
    if exact_count > 1 {
        return Err(anyhow::anyhow!(
            "edit_file: old_str matches {exact_count} locations in '{path}'. \
             It must match exactly once. \
             Add more surrounding context (e.g. the function signature above, \
             or a unique comment nearby) to make old_str unambiguous."
        ));
    }

    // 2. Fuzzy match — try whitespace normalisations, accept only if exactly one candidate
    if let Some((matched_span, label)) = fuzzy_find(&content, old_str) {
        let edit_byte = content.find(&matched_span).unwrap_or(0);
        let anchor_line = content[..edit_byte].lines().count() + 1;
        let new_content = content.replacen(&matched_span, new_str, 1);
        fs::write(path, &new_content)
            .with_context(|| format!("edit_file: cannot write '{path}'"))?;
        let ctx = post_edit_context(path, anchor_line);
        return Ok(format!("✓ Edited {path} (fuzzy match — {label}){ctx}"));
    }

    // 3. No match — return a useful ±15-line context around the best candidate line
    let hint = best_match_context(&content, old_str);
    Err(anyhow::anyhow!(
        "edit_file: string not found in '{path}'.\n\
         Check whitespace and exact characters.\n\
         {hint}"
    ))
}

/// Try whitespace-normalised matches in order of aggressiveness.
/// Returns `(actual_span_in_file, label)` if exactly one candidate found.
fn fuzzy_find(content: &str, old_str: &str) -> Option<(String, &'static str)> {
    // Strategy 1: CRLF → LF normalisation on both sides
    let content_lf = content.replace("\r\n", "\n");
    let old_lf = old_str.replace("\r\n", "\n");
    if content_lf != *content {
        // File has CRLF — check if normalised match works
        if let Some(span) = single_match(&content_lf, &old_lf) {
            // Map back to original content span (CRLF version)
            let crlf_span = span.replace('\n', "\r\n");
            if content.matches(&crlf_span).count() == 1 {
                return Some((crlf_span, "CRLF normalised"));
            }
        }
    }

    // Strategy 2: per-line trim() normalisation
    if let Some(span) = line_normalised_match(content, old_str, |l| l.trim()) {
        return Some((span, "whitespace trimmed"));
    }

    // Strategy 3: per-line trim_end() — trailing spaces only
    if let Some(span) = line_normalised_match(content, old_str, |l| l.trim_end()) {
        return Some((span, "trailing whitespace trimmed"));
    }

    None
}

/// Find a match where each line of old_str is compared after applying `norm`.
/// Returns the actual span from `content` if exactly one candidate is found.
fn line_normalised_match<'a, F>(content: &'a str, old_str: &str, norm: F) -> Option<String>
where
    F: Fn(&str) -> &str,
{
    let old_lines: Vec<&str> = old_str.lines().collect();
    if old_lines.is_empty() {
        return None;
    }
    let old_normalised: Vec<&str> = old_lines.iter().map(|l| norm(l)).collect();
    let n = old_lines.len();

    let content_lines: Vec<&str> = content.lines().collect();
    let mut candidates: Vec<(usize, usize)> = Vec::new(); // (start_line, end_line)

    'outer: for start in 0..content_lines.len().saturating_sub(n - 1) {
        for (i, old_norm) in old_normalised.iter().enumerate() {
            if norm(content_lines[start + i]) != *old_norm {
                continue 'outer;
            }
        }
        candidates.push((start, start + n));
    }

    if candidates.len() != 1 {
        return None;
    }

    let (start, end) = candidates[0];
    // Reconstruct the actual span from content_lines (preserves original whitespace)
    let span = content_lines[start..end].join("\n");
    // Confirm it appears exactly once in content
    if content.matches(span.as_str()).count() == 1 {
        Some(span)
    } else {
        None
    }
}

/// Find a single exact match of `needle` in `haystack`. Returns the match if count == 1.
fn single_match<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    if haystack.matches(needle).count() == 1 {
        let pos = haystack.find(needle)?;
        Some(&haystack[pos..pos + needle.len()])
    } else {
        None
    }
}

/// Build a ±15 line context hint around the line most similar to the first line of old_str.
fn best_match_context(content: &str, old_str: &str) -> String {
    let target = old_str.lines().next().unwrap_or("").trim();
    if target.is_empty() {
        return "Use read_file to verify the content first.".to_string();
    }

    let lines: Vec<&str> = content.lines().collect();
    // Find the line with the most chars in common (simple heuristic)
    let best = lines.iter().enumerate().max_by_key(|(_, l)| {
        let l_trim = l.trim();
        common_prefix_len(l_trim, target)
    });

    let Some((best_idx, _)) = best else {
        return "Use read_file to verify the content first.".to_string();
    };

    let lo = best_idx.saturating_sub(15);
    let hi = (best_idx + 15).min(lines.len());
    let context: String = lines[lo..hi]
        .iter()
        .enumerate()
        .map(|(i, l)| crate::tools::read::format_line(lo + i + 1, l))
        .collect();

    format!(
        "Nearest match around line {} (use these hashes for anchor):\n{}",
        best_idx + 1,
        context
    )
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

// ── Post-edit context echo ─────────────────────────────────────────────────────

/// Read the freshly-written file and return a ±10 line window centred on
/// `anchor_line` (1-indexed), formatted with hashes so the model can
/// immediately use them for the next edit without a separate read_file call.
///
/// For append mode pass `anchor_line = total_lines - appended_lines / 2`
/// (i.e. the middle of the appended block).
fn post_edit_context(path: &str, anchor_line: usize) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    // Clamp anchor to valid range (0-based internally)
    let centre = anchor_line.saturating_sub(1).min(total - 1);
    let lo = centre.saturating_sub(10);
    let hi = (centre + 10).min(total);

    let mut out = format!(
        "\n[{path} after edit — lines {}-{} of {total}]\n",
        lo + 1, hi
    );
    for (i, line) in lines[lo..hi].iter().enumerate() {
        out.push_str(&crate::tools::read::format_line(lo + i + 1, line));
    }
    out
}
