/// Compressed message history.
///
/// After each tool call round-trip we replace the full tool output in
/// conversation history with a one-line summary. The original output is
/// kept in a side-store so it can be recalled if the model asks. This
/// keeps the context window lean without losing information.
// ── Public summary type ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolRecord {
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
    pub fn record(&mut self, tool_name: &str, full_output: &str) -> (String, String) {
        let model_output = summarise(tool_name, full_output);
        let display_summary = display_summarise(tool_name, full_output);
        self.records.push(ToolRecord {
            tool_name: tool_name.to_string(),
            full_output: full_output.to_string(),
            summary: model_output.clone(),
        });
        (model_output, display_summary)
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

/// Max lines of a read_file result kept in conversation history.
/// Kept deliberately small — the model only needs enough to find its edit anchor.
/// Full content lives in the recall side-store; use line_range for deeper reads.
/// 50 lines ≈ 200 tokens vs 600 tokens at 150 — saves ~400 tokens per read per turn.
const READ_FILE_CONTEXT_LINES: usize = 50;

fn summarise(tool_name: &str, output: &str) -> String {
    match tool_name {
        // Cap read_file in the messages array.
        // Ranged reads (line_range=[N,M]) are never truncated — the model explicitly
        // requested that window and may need any line in it for editing. Truncating
        // causes re-reads and grep spirals.
        // Full-file reads of large files are capped — the symbol index in the header
        // is the useful part; the model should use line_range for depth.
        "read_file" => {
            let lines: Vec<&str> = output.lines().collect();
            if lines.len() <= READ_FILE_CONTEXT_LINES + 1 {
                return output.to_string();
            }
            // Detect ranged read by header: "[path — lines N-M of T...]"
            let is_ranged = lines.first()
                .map(|h| h.contains("— lines ") && h.contains(" of "))
                .unwrap_or(false);
            if is_ranged {
                return output.to_string();
            }
            let kept = lines[..READ_FILE_CONTEXT_LINES].join("\n");
            let omitted = lines.len() - READ_FILE_CONTEXT_LINES;
            format!("{kept}\n[+{omitted} lines omitted — re-read with line_range if needed]")
        }
        "write_file" | "edit_file" | "patch_file" => {
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
        "bash" => summarise_bash(output),
        // project_index: keep summary injection in full (it's short, ~350 tokens).
        // Drill-down results (cluster, symbols, hotspots) are capped — they can be
        // large and the model already consumed them; recall is available if needed.
        "project_index" => {
            let lines: Vec<&str> = output.lines().collect();
            if lines.len() <= READ_FILE_CONTEXT_LINES + 1 {
                return output.to_string();
            }
            let kept = lines[..READ_FILE_CONTEXT_LINES].join("\n");
            let omitted = lines.len() - READ_FILE_CONTEXT_LINES;
            format!("{kept}\n[+{omitted} lines omitted — use project_index again or recall]")
        }
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

/// bash: context-aware summarisation.
/// - Short output (≤8 lines): keep in full — pwd, echo, short commands
/// - Compiler/test error output: keep diagnostic lines (up to 30)
/// - Everything else: keep first 5 + last 3 lines
///
/// Error detection is intentionally strict — matches compiler output format only.
/// `contains("error:")` anywhere would false-positive on Rust source code passed
/// through grep/rg, inflating stored history dramatically.
fn summarise_bash(output: &str) -> String {
    const KEEP_FULL_THRESHOLD: usize = 8;
    const MAX_ERROR_LINES: usize = 30;
    const SUCCESS_HEAD: usize = 5;
    const SUCCESS_TAIL: usize = 3;

    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= KEEP_FULL_THRESHOLD {
        return output.to_string();
    }

    // Detect compiler/test diagnostic output — strict patterns that don't
    // false-positive on Rust/JS source code piped through grep or rg.
    // Compiler format:  "error[E0425]: ..."  "warning[..."  " --> src/foo.rs:42"
    // Test format:      "test foo ... FAILED"  "FAILED"  "thread '...' panicked"
    let is_compiler_line = |l: &str| -> bool {
        let t = l.trim_start();
        t.starts_with("error[") || t.starts_with("error: ") || t.starts_with("error:")
            && t[6..].starts_with(' ')  // "error: " not "error_handling::"
            || t.starts_with("warning[")
            || t.starts_with(" --> ")   // source location pointer
            || t.starts_with("thread '") && t.contains("panicked")
            || t == "FAILED"
            || t.starts_with("test ") && t.ends_with("FAILED")
            || t.starts_with("failures:")
    };

    let error_lines: Vec<&str> = lines.iter()
        .copied()
        .filter(|l| is_compiler_line(l))
        .collect();

    if !error_lines.is_empty() {
        let kept: Vec<&str> = error_lines.into_iter().take(MAX_ERROR_LINES).collect();
        let result = kept.join("\n");
        let remaining = lines.len().saturating_sub(kept.len());
        if remaining > 0 {
            return format!("{result}\n[+{remaining} lines omitted]");
        }
        return result;
    }

    // Success path — head + tail captures preamble and result summary.
    // e.g. cargo test: compilation header at top, "test result: ok." at bottom.
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
        let (model_out, display_out) = history.record("read_file", "file content here");
        
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].tool_name, "read_file");
        assert_eq!(history.records[0].full_output, "file content here");
        // Model output for read_file is full content
        assert_eq!(model_out, "file content here");
        // Display summary is shortened
        assert!(display_out.starts_with("✓ Read"));
    }


    #[test]
    fn test_compressed_count() {
        let mut history = History::default();

        // Small read_file — not compressed (under cap)
        history.record("read_file", &"x".repeat(100));
        // Large read_file — compressed (over READ_FILE_CONTEXT_LINES)
        let large_read = (1..=300).map(|i| format!("line {i}\n")).collect::<String>();
        history.record("read_file", &large_read);
        // bash with long output gets summarized
        let long_bash = (0..50).map(|i| format!("output line {}\n", i)).collect::<String>();
        history.record("bash", &long_bash);

        assert_eq!(history.compressed_count(), 2); // large read + bash
    }

    #[test]
    fn test_compress_reads_for() {
        let mut history = History::default();

        // Create a read_file record with long content (>200 chars summary)
        let long_output = "[src/main.rs — 100 lines]\n".to_string() + &"line x\n".repeat(50);
        history.record("read_file", &long_output);

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
    fn test_summarise_read_file_small_keeps_full() {
        let output = "[src/lib.rs — 50 lines]\nline 1\nline 2";
        let result = summarise("read_file", output);
        assert_eq!(result, output); // small file — kept in full
    }

    #[test]
    fn test_summarise_read_file_large_caps_at_limit() {
        let header = "[src/big.rs — 500 lines]";
        let body: String = (1..=300).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let output = format!("{header}\n{body}");
        let result = summarise("read_file", &output);
        let line_count = result.lines().count();
        // header + 50 content lines + 1 omission note = 52 lines
        assert!(line_count <= READ_FILE_CONTEXT_LINES + 2, "got {line_count} lines");
        assert!(result.contains("lines omitted"), "should have omission note");
        // lines[..50]: index 0 = header, 1..50 = "line 1".."line 49"
        assert!(result.contains("line 49"), "should include line 49");
        assert!(!result.contains("line 51"), "should not include line 51");
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
    fn test_summarise_bash_short_keeps_full() {
        let output = "line1\nline2\nline3";
        let result = summarise_bash(output);
        assert_eq!(result, output);
    }

    #[test]
    fn test_summarise_bash_error_keeps_diagnostics() {
        // Must be >8 lines to trigger summarisation. Uses strict compiler format.
        let output = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\n\
                      error[E0425]: cannot find value `x`\n \
                       --> src/main.rs:10:5\n  |\n10 |     x\n  |     ^ not found\n\
                      warning[unused]: variable `y`\n\
                      error: aborting due to 1 error";
        let result = summarise_bash(output);
        assert!(result.contains("error[E0425]"), "should keep compiler error");
        assert!(result.contains("warning["), "should keep compiler warning");
        // 'note: help:' style (without bracket) is NOT kept — avoids false positives
        // on source code containing "note:" as a string
    }

    #[test]
    fn test_summarise_bash_grep_no_false_positive() {
        // grep/rg across Rust source — must NOT trigger error path even though
        // source lines contain "error:", "warning:", "cannot", "note:" as code text
        let output = (0..20).map(|i| format!("src/foo.rs:{i}: fn handle_error() {{\n")).collect::<String>()
            + &(0..5).map(|i| format!("src/bar.rs:{i}: // warning: this is a comment\n")).collect::<String>();
        let result = summarise_bash(&output);
        // Success path: 5 head + 3 tail, not 30 error lines
        let line_count = result.lines().count();
        assert!(line_count <= 10, "grep output should not inflate to 30 lines, got {line_count}");
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
        // Short bash output stays full (≤8 lines), so it contains the original
        assert_eq!(result, output);
    }
}