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

    /// Count records where the summary is actually shorter than the full output.
    pub fn compressed_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| r.summary.len() < r.full_output.len())
            .count()
    }

    /// Compress stale read_file records for a given path.
    /// Called after a successful edit — evict stale read_file data for this path.
    /// Both the summary (in-context) and full_output (recall store) are replaced.
    /// Stale content is actively harmful: wrong line numbers, wrong hashes,
    /// wrong code — the model must re-read to get current state.
    pub fn compress_reads_for(&mut self, path: &str) {
        let stub = format!("[Stale — {path} was edited. Re-read for current content.]");
        for rec in &mut self.records {
            if rec.tool_name == "read_file" && rec.summary.contains(path) && rec.summary.len() > 200 {
                rec.summary = stub.clone();
                rec.full_output = stub.clone();
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
            // Build check failure: keep the full output so the model sees
            // compile errors and can fix them.
            if output.contains("⚠ FILE WRITTEN BUT BUILD BROKEN") || output.contains("✗ build check failed") {
                output.to_string()
            } else {
                // On success: keep the confirmation line + a brief summary of
                // what was structurally changed (new functions, structs, etc.)
                // extracted from the post-edit echo. This lets recall answer
                // "what was added?" without keeping stale hashes/line numbers.
                summarise_edit(output)
            }
        }
        // Keep full tree — essential for cross-file reasoning and project navigation.
        // Budget enforcement will compress it later if context gets tight.
        "list_files" => summarise_list(output),
        "search" => summarise_search(output),
        "bash" => summarise_bash(output),
        _ => truncate_to_lines(output, 3),
    }
}

/// edit_file / write_file success summary.
/// Keeps the confirmation line (✓ Edited ...) and extracts structural
/// information from the post-edit echo without keeping stale hashes/line numbers.
/// This gives recall enough to answer "what was added/changed?" while staying
/// compact and hash-free.
fn summarise_edit(output: &str) -> String {
    let first = first_line(output);
    
    // Extract meaningful code identifiers from the post-edit echo lines.
    // Echo lines look like "  42 [a3f2] | pub fn foo_bar(..." — strip the
    // line number and hash, keep function/struct/impl/test declarations.
    let mut new_symbols: Vec<String> = Vec::new();
    for line in output.lines().skip(1) {
        // Strip "  NNN [hash] | " prefix to get the actual code
        let code = if let Some(pos) = line.find(" | ") {
            line[pos + 3..].trim()
        } else {
            continue;
        };

        // Extract declarations — these tell the next step what exists
        let sym = if code.starts_with("pub fn ") || code.starts_with("fn ") {
            code.split('(').next().map(|s| s.to_string())
        } else if code.starts_with("pub struct ") || code.starts_with("struct ") {
            code.split('{').next().or(code.split(';').next()).map(|s| s.trim().to_string())
        } else if code.starts_with("pub enum ") || code.starts_with("enum ") {
            code.split('{').next().map(|s| s.trim().to_string())
        } else if code.starts_with("impl ") {
            code.split('{').next().map(|s| s.trim().to_string())
        } else if code.starts_with("#[cfg(test)]") {
            Some("#[cfg(test)] mod tests".to_string())
        } else if code.starts_with("mod ") {
            code.split('{').next().map(|s| s.trim().to_string())
        } else if code.starts_with("fn test_") || code.starts_with("async fn test_") {
            code.split('(').next().map(|s| s.to_string())
        } else if code.starts_with("export function ") || code.starts_with("function ") {
            code.split('(').next().map(|s| s.to_string())
        } else if code.starts_with("class ") {
            code.split('{').next().or(code.split('(').next()).map(|s| s.trim().to_string())
        } else if code.starts_with("def ") {
            code.split('(').next().map(|s| s.to_string())
        } else {
            None
        };

        if let Some(s) = sym {
            if !s.is_empty() && new_symbols.len() < 10 {
                new_symbols.push(s);
            }
        }
    }

    if new_symbols.is_empty() {
        first.to_string()
    } else {
        format!("{first}\n  added/modified: {}", new_symbols.join(", "))
    }
}


/// list_files: keep full tree if ≤80 lines (essential for cross-file reasoning),
/// otherwise keep only directory names + entry count.
fn summarise_list(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    // ≤80 lines: keep everything — the model needs filename awareness for
    // navigation, test discovery, cross-file editing, etc.
    if lines.len() <= 80 {
        return output.to_string();
    }

    // Large tree: keep directory structure (lines ending with /) + summary line
    let mut out = String::new();
    for line in &lines {
        let trimmed = line.trim();
        // Keep directory lines, the header, the count footer, and blank lines
        if trimmed.ends_with('/') || trimmed.starts_with('[') || trimmed.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    let file_count = lines.len() - out.lines().count();
    if file_count > 0 {
        out.push_str(&format!("[{file_count} files omitted — directories shown above. Ask to recall for full listing.]"));
    }
    out
}

/// search: keep matched lines (the actual code content is essential for
/// cross-file reasoning). Cap at 30 match lines to stay bounded.
fn summarise_search(output: &str) -> String {
    if output.starts_with("No matches") {
        return output.lines().next().unwrap_or("No matches").to_string();
    }

    let lines: Vec<&str> = output.lines().collect();

    // Small results: keep everything
    if lines.len() <= 30 {
        return output.to_string();
    }

    // Large results: keep first 25 match lines + count footer
    // Match lines have the format "file.rs:12:content" or "file.rs-12-content"
    let match_lines: Vec<&str> = lines
        .iter()
        .filter(|l| {
            let parts: Vec<&str> = l.splitn(3, ':').collect();
            parts.len() >= 2 && parts[1].parse::<u32>().is_ok()
        })
        .copied()
        .collect();

    let total = match_lines.len();
    if total == 0 {
        return truncate_to_lines(output, 5);
    }

    let kept: Vec<&str> = match_lines.into_iter().take(25).collect();
    let mut result = kept.join("\n");
    let remaining = total.saturating_sub(25);
    if remaining > 0 {
        result.push_str(&format!("\n[+{remaining} matches — ask to recall for full results]"));
    }
    result
}

/// bash: context-aware summarisation.
/// - Short output (≤20 lines): keep in full
/// - Error/failure lines: keep all diagnostics (up to 30)
/// - Success: keep first 10 + last 5 lines (captures both preamble and result summary)
fn summarise_bash(output: &str) -> String {
    const KEEP_FULL_THRESHOLD: usize = 20;
    const MAX_ERROR_LINES: usize = 30;
    const SUCCESS_HEAD: usize = 10;
    const SUCCESS_TAIL: usize = 5;

    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= KEEP_FULL_THRESHOLD {
        return output.to_string();
    }

    // Collect lines that indicate errors or failures
    let error_lines: Vec<&str> = lines.iter()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error:") || l.contains("error[")
                || l.contains("failed") || l.contains("fail:")
                || l.contains("panic") || l.contains("warning:")
                || l.contains("cannot") || l.contains("note:")
        })
        .copied()
        .collect();

    if !error_lines.is_empty() {
        // Error path — keep all diagnostic lines (capped)
        let kept: Vec<&str> = error_lines.into_iter().take(MAX_ERROR_LINES).collect();
        let result = kept.join("\n");
        let remaining = lines.len().saturating_sub(kept.len());
        if remaining > 0 {
            return format!("{result}\n[+{remaining} lines — ask to recall for full output]");
        }
        return result;
    }

    // Success path — head + tail captures command setup and result summary
    // (e.g. cargo test: compilation at top, "test result: ok" at bottom)
    let head = &lines[..SUCCESS_HEAD];
    let tail_start = lines.len().saturating_sub(SUCCESS_TAIL);
    let tail = &lines[tail_start..];
    let omitted = tail_start.saturating_sub(SUCCESS_HEAD);

    let mut result = head.join("\n");
    if omitted > 0 {
        result.push_str(&format!("\n[... {omitted} lines omitted ...]"));
    }
    result.push('\n');
    result.push_str(&tail.join("\n"));
    result
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── History Store ───────────────────────────────────────────────────────

    #[test]
    fn test_history_record_stores_and_returns_summaries() {
        let mut history = History::default();
        let (model_out, display_out) = history.record("call-1", "read_file", "file content here");
        
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].tool_call_id, "call-1");
        assert_eq!(history.records[0].tool_name, "read_file");
        assert_eq!(history.records[0].full_output, "file content here");
        // Model output for read_file is full content
        assert_eq!(model_out, "file content here");
        // Display summary is shortened
        assert!(display_out.starts_with("✓ Read"));
    }

    #[test]
    fn test_history_recall_by_id() {
        let mut history = History::default();
        history.record("call-1", "bash", "full output");
        
        assert_eq!(history.recall("call-1"), Some("full output"));
        assert_eq!(history.recall("nonexistent"), None);
    }

    #[test]
    fn test_history_recall_by_name() {
        let mut history = History::default();
        history.record("call-1", "bash", "first output");
        history.record("call-2", "read_file", "second output");
        history.record("call-3", "bash", "third output");
        
        // Should recall most recent bash
        assert_eq!(history.recall_by_name("bash"), Some("third output"));
        assert_eq!(history.recall_by_name("read_file"), Some("second output"));
        assert_eq!(history.recall_by_name("unknown"), None);
    }

    #[test]
    fn test_compressed_count() {
        let mut history = History::default();

        // read_file returns full output (not compressed)
        history.record("c1", "read_file", &"x".repeat(1000));
        // bash with long output gets summarized (shorter than full output)
        // The summarise_bash function keeps head+tail for long success outputs
        let long_bash = (0..50).map(|i| format!("output line {}\n", i)).collect::<String>();
        history.record("c2", "bash", &long_bash);

        assert_eq!(history.compressed_count(), 1);
    }

    #[test]
    fn test_compress_reads_for() {
        let mut history = History::default();

        // Create a read_file record with long content (>200 chars summary)
        let long_output = "[src/main.rs — 100 lines]\n".to_string() + &"line x\n".repeat(50);
        history.record("c1", "read_file", &long_output);

        let summary_len = history.records[0].summary.len();
        assert!(summary_len > 200, "summary should be >200 chars for this test");
        assert!(!history.records[0].summary.contains("Stale"));

        // Compress for the path
        history.compress_reads_for("src/main.rs");

        assert!(history.records[0].summary.contains("Stale"));
        assert!(history.records[0].full_output.contains("Stale"));
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    #[test]
    fn test_first_line() {
        assert_eq!(first_line("hello\nworld"), "hello");
        assert_eq!(first_line("single"), "single");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn test_truncate_to_lines() {
        assert_eq!(truncate_to_lines("a\nb\nc", 5), "a\nb\nc");
        assert_eq!(truncate_to_lines("a\nb\nc", 2), "a\nb\n[+1 lines truncated]");
        assert_eq!(truncate_to_lines("a\nb", 2), "a\nb");
    }

    // ── Summarisation ───────────────────────────────────────────────────────

    #[test]
    fn test_summarise_read_file_keeps_content() {
        let output = "[src/lib.rs — 50 lines]\nline 1\nline 2";
        let result = summarise("read_file", output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_edit_build_failure_keeps_output() {
        let output = "⚠ FILE WRITTEN BUT BUILD BROKEN\nerror: something";
        let result = summarise("edit_file", output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_edit_success_summarises() {
        let output = "✓ Edited src/main.rs\n  10 [a1b2] | pub fn new_func() {\n  11 [c3d4] | }";
        let result = summarise("edit_file", output);
        assert!(result.contains("✓ Edited"));
        assert!(result.contains("new_func"));
    }

    #[test]
    fn test_summarise_list_small_keeps_full() {
        let output = "src/\n  main.rs\n  lib.rs";
        let result = summarise_list(output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_list_large_keeps_directories() {
        let mut large = String::new();
        for i in 0..100 {
            large.push_str(&format!("src/module{}/\n", i));
        }
        large.push_str("  file1.rs\n"); // file, not directory
        
        let result = summarise_list(&large);
        assert!(result.contains("src/module0/"));
        assert!(!result.contains("file1.rs"));
        assert!(result.contains("files omitted"));
    }

    #[test]
    fn test_summarise_search_no_matches() {
        let output = "No matches found in project";
        let result = summarise_search(output);
        assert_eq!(result, "No matches found in project");
    }

    #[test]
    fn test_summarise_search_small_keeps_full() {
        let output = "src/a.rs:1:fn foo() {}\nsrc/b.rs:2:fn bar() {}";
        let result = summarise_search(output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_search_large_truncates() {
        let mut large = String::new();
        for i in 1..=50 {
            large.push_str(&format!("src/f{}.rs:{i}:fn test_{i}() {{\n", i));
        }
        
        let result = summarise_search(&large);
        assert!(result.contains("test_1"));
        assert!(!result.contains("test_50")); // should be omitted
        assert!(result.contains("matches"));
    }

    #[test]
    fn test_summarise_bash_short_keeps_full() {
        let output = "line1\nline2\nline3";
        let result = summarise_bash(output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_bash_error_keeps_diagnostics() {
        let output = "compiling...\nwarning: unused variable\nerror[E0425]: cannot find value\nnote: help: try using\nmore output";
        let result = summarise_bash(output);
        // Error keywords are kept
        assert!(result.contains("error[E0425]"));
        assert!(result.contains("warning:"));
        assert!(result.contains("note:")); // note: is kept as diagnostic
    }

    #[test]
    fn test_summarise_bash_success_head_tail() {
        let mut output = String::new();
        for i in 1..=30 {
            output.push_str(&format!("output line {}\n", i));
        }
        
        let result = summarise_bash(&output);
        assert!(result.contains("output line 1"));
        assert!(result.contains("output line 30"));
        assert!(result.contains("omitted"));
    }

    #[test]
    fn test_display_summarise_read_file() {
        let output = "[src/main.rs — 42 lines]\n  1 [hash] | fn main()\n  2 [hash] | }";
        let result = display_summarise("read_file", output);
        assert!(result.contains("src/main.rs"), "should contain path");
        assert!(result.contains("lines shown"), "should mention lines shown");
    }

    #[test]
    fn test_display_summarise_other_uses_summarise() {
        let output = "some bash output";
        let result = display_summarise("bash", output);
        // Short bash output stays full (≤20 lines), so it contains the original
        assert_eq!(result, output);
    }
}