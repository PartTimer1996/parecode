use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "edit_file",
        "description": "Replace an exact string in a file. The old_str must match exactly (whitespace included).",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "old_str": {
                    "type": "string",
                    "description": "Exact string to find and replace"
                },
                "new_str": {
                    "type": "string",
                    "description": "Replacement string"
                }
            },
            "required": ["path", "old_str", "new_str"]
        }
    })
}

pub fn execute(args: &Value) -> Result<String> {
    let path = args["path"].as_str().context("edit_file: missing 'path'")?;
    let old_str = args["old_str"]
        .as_str()
        .context("edit_file: missing 'old_str'")?;
    let new_str = args["new_str"]
        .as_str()
        .context("edit_file: missing 'new_str'")?;

    let content = fs::read_to_string(path)
        .with_context(|| format!("edit_file: cannot read '{path}'"))?;

    // 1. Exact match
    let exact_count = content.matches(old_str).count();
    if exact_count > 0 {
        let new_content = content.replacen(old_str, new_str, exact_count);
        fs::write(path, &new_content)
            .with_context(|| format!("edit_file: cannot write '{path}'"))?;
        return Ok(format!("✓ Edited {path} ({exact_count} replacement{})", if exact_count == 1 { "" } else { "s" }));
    }

    // 2. Fuzzy match — try whitespace normalisations, accept only if exactly one candidate
    if let Some((matched_span, label)) = fuzzy_find(&content, old_str) {
        let new_content = content.replacen(&matched_span, new_str, 1);
        fs::write(path, &new_content)
            .with_context(|| format!("edit_file: cannot write '{path}'"))?;
        return Ok(format!("✓ Edited {path} (fuzzy match — {label})"));
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
    let context: Vec<String> = lines[lo..hi]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>4} | {l}", lo + i + 1))
        .collect();

    format!(
        "Nearest match around line {}:\n{}",
        best_idx + 1,
        context.join("\n")
    )
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}
