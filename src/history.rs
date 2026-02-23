/// Compressed message history.
///
/// After each tool call round-trip we replace the full tool output in
/// conversation history with a one-line summary. The original output is
/// kept in a side-store so it can be recalled if the model asks. This
/// keeps the context window lean without losing information.
// ── Public summary type ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolRecord {
    pub tool_call_id: String,
    pub tool_name: String,
    /// The full, untruncated output — stored off-context
    pub full_output: String,
    /// One-liner that goes into conversation history
    pub summary: String,
}

// ── History store ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct History {
    records: Vec<ToolRecord>,
}

impl History {
    /// Record a completed tool call and produce the summary that will be
    /// sent back to the model as the tool result.
    /// Record a completed tool call.
    /// Returns `(model_output, display_summary)`:
    /// - `model_output` is what goes into the conversation history sent to the model
    /// - `display_summary` is a short one-liner for the TUI sidebar
    pub fn record(&mut self, tool_call_id: &str, tool_name: &str, full_output: &str) -> (String, String) {
        let model_output = summarise(tool_name, full_output);
        let display_summary = display_summarise(tool_name, full_output);
        self.records.push(ToolRecord {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            full_output: full_output.to_string(),
            summary: model_output.clone(),
        });
        (model_output, display_summary)
    }

    /// Recall the full output for a given tool_call_id (if it exists).
    pub fn recall(&self, tool_call_id: &str) -> Option<&str> {
        self.records
            .iter()
            .find(|r| r.tool_call_id == tool_call_id)
            .map(|r| r.full_output.as_str())
    }

    /// Recall the most recent full output for a given tool name.
    pub fn recall_by_name(&self, tool_name: &str) -> Option<&str> {
        self.records
            .iter()
            .rfind(|r| r.tool_name == tool_name)
            .map(|r| r.full_output.as_str())
    }

    /// All records (for diagnostics / verbose output).
    pub fn records(&self) -> &[ToolRecord] {
        &self.records
    }

    /// Count records where the summary is actually shorter than the full output.
    pub fn compressed_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| r.summary.len() < r.full_output.len())
            .count()
    }

    /// Compress stale read_file records for a given path.
    /// Called after a successful edit_file — the old read content has stale
    /// hashes/line numbers, so keeping it verbatim wastes context.
    /// The full_output is preserved for recall; only the summary (what goes
    /// into conversation history) is replaced with a short note.
    pub fn compress_reads_for(&mut self, path: &str) {
        for rec in &mut self.records {
            if rec.tool_name == "read_file" && rec.summary.contains(path) && rec.summary.len() > 200 {
                rec.summary = format!(
                    "[Previously read {path} — content is now stale after edit. \
                     Use read_file to get current content if needed.]"
                );
            }
        }
    }
}

// ── Summarisation rules (deterministic, zero model calls) ────────────────────

/// One-liner for the TUI sidebar — always short regardless of tool.
fn display_summarise(tool_name: &str, output: &str) -> String {
    match tool_name {
        "read_file" => {
            // Extract path and line count from the header line "[path — N lines...]"
            let first = first_line(output);
            if first.starts_with('[') {
                let inner = first.trim_start_matches('[');
                let path_part = inner
                    .split(" —")
                    .next()
                    .unwrap_or(inner)
                    .trim_end_matches(']');
                let content_lines = output.lines().filter(|l| l.contains(" | ")).count();
                if content_lines > 0 {
                    return format!("✓ Read {path_part} ({content_lines} lines shown)");
                }
                return format!("✓ Read {path_part}");
            }
            format!("✓ Read file ({} lines)", output.lines().count())
        }
        _ => summarise(tool_name, output),
    }
}

fn summarise(tool_name: &str, output: &str) -> String {
    match tool_name {
        // Keep read_file content in context — the model needs it to write correct
        // old_str values for edit_file. Budget enforcement will compress it if
        // the context window fills up.
        "read_file" => output.to_string(),
        "write_file" | "edit_file" => {
            // Build check failure: starts with "⚠ FILE WRITTEN BUT BUILD BROKEN"
            // Keep the full output so the model sees compile errors.
            if output.contains("⚠ FILE WRITTEN BUT BUILD BROKEN") || output.contains("✗ build check failed") {
                output.to_string()
            } else {
                // On success: keep the first line (✓ Edited ...) plus any
                // post-edit context echo (the ±10-line excerpt with fresh hashes).
                // The excerpt is what lets the model make follow-up edits without
                // re-reading — stripping it defeats its purpose.
                output.to_string()
            }
        }
        "list_files" => summarise_list(output),
        "search" => summarise_search(output),
        "bash" => summarise_bash(output),
        _ => truncate_to_lines(output, 3),
    }
}


/// list_files: "✓ Listed src/: 24 entries"
fn summarise_list(output: &str) -> String {
    // Our list output ends with "[N entries]" or "[Truncated...]"
    if let Some(last) = output.lines().last() {
        if last.starts_with('[') {
            // Extract the path from first line if present
            let path = output
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().next())
                .unwrap_or(".");
            return format!("✓ Listed {path}: {}", last.trim_start_matches('[').trim_end_matches(']'));
        }
    }
    let count = output.lines().filter(|l| l.contains("──")).count();
    format!("✓ Listed directory ({count} entries)")
}

/// search: "✓ search('pattern') → 7 matches: file.ts:12, file.ts:45, ..."
fn summarise_search(output: &str) -> String {
    if output.starts_with("No matches") {
        return output.lines().next().unwrap_or("No matches").to_string();
    }

    // Count match lines (lines with ":" separating file:line:content)
    let match_lines: Vec<&str> = output
        .lines()
        .filter(|l| {
            // rg output: "file.ts:12:content" or "file.ts:12-content" (context lines)
            let parts: Vec<&str> = l.splitn(3, ':').collect();
            parts.len() >= 2 && parts[1].parse::<u32>().is_ok()
        })
        .collect();

    let n = match_lines.len();
    if n == 0 {
        return truncate_to_lines(output, 2);
    }

    // Collect unique file:line pairs (up to 5 for the summary)
    let mut locations: Vec<String> = match_lines
        .iter()
        .filter_map(|l| {
            let mut parts = l.splitn(3, ':');
            let file = parts.next()?;
            let line = parts.next()?;
            Some(format!("{file}:{line}"))
        })
        .collect::<std::collections::LinkedList<_>>()  // dedup-friendly
        .into_iter()
        .collect::<Vec<_>>();
    locations.dedup();

    let shown: Vec<&str> = locations.iter().take(5).map(String::as_str).collect();
    let tail = if locations.len() > 5 {
        format!(", +{} more", locations.len() - 5)
    } else {
        String::new()
    };

    format!("✓ search → {n} matches: {}{tail}", shown.join(", "))
}

/// bash: error-line aware summarisation.
/// - If error/failure lines exist: emit them (up to 20) + recall hint
/// - Otherwise: emit first 5 lines (success case)
/// - Cap at 25 lines total
fn summarise_bash(output: &str) -> String {
    const MAX_SUMMARY: usize = 25;
    const MAX_ERROR_LINES: usize = 20;
    const SUCCESS_HEAD: usize = 5;

    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= SUCCESS_HEAD {
        return output.to_string();
    }

    // Collect lines that indicate errors or failures
    let error_lines: Vec<(usize, &&str)> = lines.iter().enumerate()
        .filter(|(_, l)| {
            let l = l.to_ascii_lowercase();
            l.contains("error:") || l.contains("error[")
                || l.contains("failed") || l.contains("fail:")
                || l.contains("panic") || l.contains("warning:")
                || l.contains("cannot") || l.contains("note:")
        })
        .collect();

    if error_lines.is_empty() {
        // Success path — first 5 lines is enough
        let head = lines[..SUCCESS_HEAD].join("\n");
        return format!("{head}\n[+{} lines — full output stored, ask to recall]", lines.len() - SUCCESS_HEAD);
    }

    // Error path — keep all diagnostic lines (capped)
    let kept: Vec<&str> = error_lines.iter()
        .take(MAX_ERROR_LINES)
        .map(|(_, l)| **l)
        .collect();
    let shown = kept.len().min(MAX_SUMMARY);
    let result = kept[..shown].join("\n");
    let remaining = lines.len().saturating_sub(shown);
    if remaining > 0 {
        format!("{result}\n[+{remaining} lines — full output stored, ask to recall]")
    } else {
        result
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn truncate_to_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= n {
        return s.to_string();
    }
    format!("{}\n[+{} lines truncated]", lines[..n].join("\n"), lines.len() - n)
}

    #[test]
    fn test_history_record() {
        let mut history = History::default();
        let (summary, display) = history.record("test_id", "test_tool", "test_output");
        assert_eq!(summary, "test_output");
        assert_eq!(display, "test_output");
        assert_eq!(history.records.len(), 1);
    }

    #[test]
    fn test_history_recall() {
        let mut history = History::default();
        history.records.push(ToolRecord {
            tool_call_id: "test_id".to_string(),
            tool_name: "test_tool".to_string(),
            full_output: "test_output".to_string(),
            summary: "summary".to_string(),
        });
        
        assert_eq!(history.recall("test_id"), Some("test_output"));
    }

    #[test]
    fn test_history_recall_by_name() {
        let mut history = History::default();
        history.records.push(ToolRecord {
            tool_call_id: "test_id".to_string(),
            tool_name: "test_tool".to_string(),
            full_output: "test_output".to_string(),
            summary: "summary".to_string(),
        });
        
        assert_eq!(history.recall_by_name("test_tool"), Some("test_output"));
    }

    #[test]
    fn test_history_records() {
        let mut history = History::default();
        history.records.push(ToolRecord {
            tool_call_id: "test_id".to_string(),
            tool_name: "test_tool".to_string(),
            full_output: "test_output".to_string(),
            summary: "summary".to_string(),
        });
        
        assert_eq!(history.records().len(), 1);
    }

    #[test]
    fn test_history_compressed_count() {
        let mut history = History::default();
        history.records.push(ToolRecord {
            tool_call_id: "test_id".to_string(),
            tool_name: "test_tool".to_string(),
            full_output: "test_output".to_string(),
            summary: "summary".to_string(),
        });
        
        assert_eq!(history.compressed_count(), 1);
    }
