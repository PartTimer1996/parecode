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
                // On success: keep only the confirmation line.
                // The post-edit ±10-line context echo was useful on the turn
                // it was produced, but becomes stale on any subsequent edit —
                // wrong hashes, wrong line numbers. Strip it here so it never
                // lingers in recall/context. The model can re-read if needed.
                first_line(output).to_string()
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