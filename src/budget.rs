/// Proactive token budget enforcement.
///
/// Unlike OpenCode (which reacts at 90% capacity with an LLM summarisation
/// call), PareCode enforces budgets *before* each API call using deterministic
/// compression — no model calls, no token cost to save tokens.
///
/// Strategy when over budget:
///   1. Compress oldest tool results further (already summarised; now inline-only)
///   2. If still over: one-sentence summaries of oldest conversation turns
///   3. Hard floor: never drop the system prompt or the original user task
use crate::client::{ContentPart, Message, MessageContent, ToolCall};

/// Tool call ID used for the synthetic PIE session-start injection.
/// Messages at the head of history with this ID are never dropped — they
/// contain the project index summary that orients the model for the session.
const PIE_INJECTION_ID: &str = "pie_ctx_0";

/// Token budget split (in tokens). Proportions match the plan.
pub struct BudgetConfig {
    pub total_context: u32,
    /// Headroom reserved for the model's response
    pub response_headroom: u32,
}

impl BudgetConfig {
    pub fn from_context_tokens(context_tokens: u32) -> Self {
        // Reserve 15% for response, leaving 85% for the conversation
        let response_headroom = (context_tokens as f32 * 0.15) as u32;
        Self {
            total_context: context_tokens,
            response_headroom,
        }
    }

    /// Maximum tokens we can use for the outgoing request
    pub fn usable(&self) -> u32 {
        self.total_context.saturating_sub(self.response_headroom)
    }

    /// Trigger compression at 80% of usable budget
    pub fn compression_threshold(&self) -> u32 {
        (self.usable() as f32 * 0.80) as u32
    }
}

// ── Token estimation (cheap approximation: 1 token ≈ 4 chars) ────────────────

pub fn estimate_tokens(s: &str) -> usize {
    // +10 overhead per message for role/formatting
    // chars().count() instead of len() — avoids overestimating multi-byte Unicode
    s.chars().count() / 4 + 10
}

pub fn estimate_messages(messages: &[Message]) -> usize {
    messages.iter().map(|m| estimate_message(m)).sum()
}

fn estimate_message(m: &Message) -> usize {
    match &m.content {
        MessageContent::Text(t) => estimate_tokens(t),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => estimate_tokens(text),
                ContentPart::ToolResult { content, .. } => estimate_tokens(content),
            })
            .sum(),
    }
}

// ── Budget enforcer ───────────────────────────────────────────────────────────

pub struct Budget {
    config: BudgetConfig,
}

impl Budget {
    pub fn new(context_tokens: u32) -> Self {
        Self {
            config: BudgetConfig::from_context_tokens(context_tokens),
        }
    }

    pub fn total_context(&self) -> u32 {
        self.config.total_context
    }

    /// Check usage and compress messages if needed.
    /// Returns (current_estimate, was_compressed).
    pub fn enforce(&self, messages: &mut Vec<Message>, system_tokens: usize) -> (usize, bool) {
        let threshold = self.config.compression_threshold() as usize;
        let current = estimate_messages(messages) + system_tokens;

        if current <= threshold {
            return (current, false);
        }

        // Pass 1: compress tool result messages further (drop repeated content)
        self.compress_tool_results(messages);

        let after_pass1 = estimate_messages(messages) + system_tokens;
        if after_pass1 <= threshold {
            return (after_pass1, true);
        }

        // Pass 2: trim oldest non-essential turns
        self.trim_oldest_turns(messages);

        let after_pass2 = estimate_messages(messages) + system_tokens;
        (after_pass2, true)
    }

    /// Replace verbose tool results with short summaries.
    /// Only compresses older messages — leaves the most recent tool turn intact
    /// so the model still has the content it just received.
    fn compress_tool_results(&self, messages: &mut Vec<Message>) {
        // Find the index of the last tool message — leave it uncompressed
        let last_tool_idx = messages
            .iter()
            .rposition(|m| m.role == "tool")
            .unwrap_or(0);

        for (idx, msg) in messages.iter_mut().enumerate() {
            if msg.role != "tool" || idx >= last_tool_idx {
                continue;
            }
            if let MessageContent::Parts(parts) = &mut msg.content {
                for part in parts.iter_mut() {
                    if let ContentPart::ToolResult { content, .. } = part {
                        // Already short enough — leave it
                        if content.len() <= 200 {
                            continue;
                        }
                        // Compress to a short summary line
                        *content = compress_tool_content(content);
                    }
                }
            }
        }
    }

    /// Drop the oldest assistant+tool turn pairs, keeping at least the
    /// first user message (task) and the last 2 turns intact.
    fn trim_oldest_turns(&self, messages: &mut Vec<Message>) {
        // Never drop: the last 4 messages (last 2 turns)
        let protected_tail = 4usize;

        if messages.len() <= protected_tail + 1 {
            return;
        }

        // Detect the PIE injection pair at the head of history.
        // Structure: [assistant(pie_ctx_0 tool call), user(tool result), user(task), ...]
        // The pair must never be dropped — it orients the model for the whole session.
        // The first *real* user task message immediately after the pair is also protected.
        let pie_head = messages
            .first()
            .map(|m| m.role == "assistant" && has_tool_call_id(&m.tool_calls, PIE_INJECTION_ID))
            .unwrap_or(false);
        // skip_to: first index we may consider dropping (after PIE pair + task message)
        let skip_to = if pie_head { 3 } else { 1 };

        // Find the first non-user message we can drop (skip_to onwards, not in tail)
        let drop_before = messages.len() - protected_tail;
        let mut drop_idx = None;

        for i in skip_to..drop_before {
            // Drop assistant messages and their following tool result blocks
            if messages[i].role == "assistant" {
                drop_idx = Some(i);
                break;
            }
        }

        if let Some(idx) = drop_idx {
            // Remove assistant message + following tool result message (if any)
            let end = if idx + 1 < messages.len() && messages[idx + 1].role == "tool" {
                idx + 2
            } else {
                idx + 1
            };
            messages.drain(idx..end);
        }
    }
}

/// Compress a tool result content string to a short summary.
/// Understands read_file header format; falls back to first line.
fn compress_tool_content(content: &str) -> String {
    let first = content.lines().next().unwrap_or(content);

    // read_file header: "[path — N lines total, showing ...]"
    if first.starts_with('[') && first.contains(" — ") {
        let inner = first.trim_start_matches('[');
        let path_part = inner
            .split(" —")
            .next()
            .unwrap_or(inner)
            .trim_end_matches(']')
            .trim();
        let line_count = content.lines().filter(|l| l.contains(" | ")).count();
        if line_count > 0 {
            return format!("[content compressed — ✓ Read {path_part} ({line_count} lines). Ask to recall if needed.]");
        }
        return format!("[content compressed — ✓ Read {path_part}. Ask to recall if needed.]");
    }

    // Already a one-liner or unknown format — keep first line
    first.to_string()
}

/// Returns true if any tool call in the list has the given ID.
fn has_tool_call_id(tool_calls: &[ToolCall], id: &str) -> bool {
    tool_calls.iter().any(|tc| tc.id == id)
}

// ── Loop detection ────────────────────────────────────────────────────────────

/// Track recent tool calls to detect doom loops.
/// Fires at 2 identical consecutive calls (vs OpenCode's 3).
#[derive(Default)]
pub struct LoopDetector {
    recent: Vec<(String, String)>, // (tool_name, args_fingerprint)
}

impl LoopDetector {
    /// Record a tool call. Returns true if a loop is detected.
    pub fn record(&mut self, tool_name: &str, args: &str) -> bool {
        // Fingerprint: tool name + truncated args.
        // Use enough of args that read_file with different line_ranges doesn't false-positive.
        // Truncate at a char boundary to avoid panicking on multi-byte UTF-8.
        let trunc = args
            .char_indices()
            .take_while(|(i, _)| *i < 400)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let fp = format!("{tool_name}::{}", &args[..trunc]);

        // Keep last 5
        self.recent.push((tool_name.to_string(), fp.clone()));
        if self.recent.len() > 5 {
            self.recent.remove(0);
        }

        // Loop = same fingerprint appears twice in recent history
        let count = self.recent.iter().filter(|(_, f)| f == &fp).count();
        count >= 2
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BudgetConfig ─────────────────────────────────────────────────────

    #[test]
    fn test_budget_config_from_tokens() {
        let config = BudgetConfig::from_context_tokens(1000);
        assert_eq!(config.total_context, 1000);
        assert_eq!(config.response_headroom, 150); // 15% of 1000
        assert_eq!(config.usable(), 850);
    }

    #[test]
    fn test_compression_threshold() {
        let config = BudgetConfig::from_context_tokens(1000);
        // usable = 850, threshold = 80% of 850 = 680
        assert_eq!(config.compression_threshold(), 680);
    }

    #[test]
    fn test_budget_config_zero_tokens() {
        let config = BudgetConfig::from_context_tokens(0);
        assert_eq!(config.usable(), 0);
        assert_eq!(config.compression_threshold(), 0);
    }

    // ── Token estimation ─────────────────────────────────────────────────

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 10); // overhead only
    }

    #[test]
    fn test_estimate_tokens_short() {
        // "hello" = 5 chars / 4 = 1 token + 10 overhead = 11
        assert_eq!(estimate_tokens("hello"), 11);
    }

    #[test]
    fn test_estimate_tokens_multibyte() {
        // "café" = 4 chars (é is 2 bytes but 1 char) / 4 = 1 + 10 = 11
        assert_eq!(estimate_tokens("café"), 11);
    }

    #[test]
    fn test_estimate_tokens_long_string() {
        let s = "a".repeat(1000);
        // 1000 chars / 4 = 250 + 10 = 260
        assert_eq!(estimate_tokens(&s), 260);
    }

    // ── compress_tool_content ────────────────────────────────────────────

    #[test]
    fn test_compress_read_file_header_with_lines() {
        let content = "[src/main.rs — 500 lines total, showing 1-50]\n  1 [hash] | fn main() {\n  2 [hash] |     println!(\"hello\");\n  3 [hash] | }";
        let result = compress_tool_content(content);
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("3 lines"));
        assert!(result.contains("compressed"));
    }

    #[test]
    fn test_compress_read_file_header_no_lines() {
        let content = "[src/main.rs — 50 lines total]\n  1 [hash] | fn main() {}";
        let result = compress_tool_content(content);
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("compressed"));
    }

    #[test]
    fn test_compress_plain_text() {
        let content = "This is just a plain text result.\nSecond line here.";
        let result = compress_tool_content(content);
        assert_eq!(result, "This is just a plain text result.");
    }

    #[test]
    fn test_compress_short_content_unchanged() {
        // Content <= 200 chars should be left unchanged
        let content = "Short result";
        let result = compress_tool_content(content);
        // This depends on the caller to check length; function itself returns first line
        assert_eq!(result, "Short result");
    }

    // ── LoopDetector ──────────────────────────────────────────────────────

    #[test]
    fn test_loop_detector_no_loop() {
        let mut detector = LoopDetector::default();
        assert!(!detector.record("read_file", r#"{"path": "a.rs"}"#));
        assert!(!detector.record("read_file", r#"{"path": "b.rs"}"#));
    }

    #[test]
    fn test_loop_detector_detects_repeat() {
        let mut detector = LoopDetector::default();
        let args = r#"{"path": "a.rs"}"#;
        assert!(!detector.record("read_file", args));
        // Same exact call should trigger loop detection
        assert!(detector.record("read_file", args));
    }

    #[test]
    fn test_loop_detector_different_args_no_loop() {
        let mut detector = LoopDetector::default();
        assert!(!detector.record("read_file", r#"{"path": "a.rs"}"#));
        assert!(!detector.record("read_file", r#"{"path": "b.rs"}"#));
        // Third different call shouldn't trigger
        assert!(!detector.record("read_file", r#"{"path": "c.rs"}"#));
    }

    #[test]
    fn test_loop_detector_different_tools() {
        let mut detector = LoopDetector::default();
        assert!(!detector.record("read_file", r#"{"path": "a.rs"}"#));
        assert!(!detector.record("edit_file", r#"{"path": "a.rs"}"#));
    }

    #[test]
    fn test_loop_detector_eviction() {
        let mut detector = LoopDetector::default();
        // Record 6 unique calls, first should be evicted
        for i in 0..6 {
            let args = format!(r#"{{"path": "{}.rs"}}"#, i);
            detector.record("read_file", &args);
        }
        // All are unique, so no loop
        let args = r#"{"path": "0.rs"}"#;
        assert!(!detector.record("read_file", args));
    }

    // ── Budget enforcement (integration) ─────────────────────────────────

    #[test]
    fn test_budget_under_threshold_no_compression() {
        let budget = Budget::new(1000); // threshold = 680
        let messages = vec![];
        let (tokens, compressed) = budget.enforce(&mut messages.clone(), 0);
        assert_eq!(compressed, false);
        assert!(tokens <= 680);
    }

    #[test]
    fn test_budget_preserves_first_message() {
        let budget = Budget::new(1000);
        let mut messages = vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::Text("My task".to_string()),
                tool_calls: vec![],
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Text("I'll help".to_string()),
                tool_calls: vec![],
            },
        ];
        budget.enforce(&mut messages, 0);
        // First message must always remain
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn test_pie_injection_pair_never_trimmed() {
        use crate::client::ToolCall;

        // Build a message list that simulates PIE injection + task + many turns
        // The budget is tiny so trim_oldest_turns will fire.
        let budget = Budget::new(200); // very small — forces compression

        let pie_assistant = Message {
            role: "assistant".to_string(),
            content: MessageContent::Text(String::new()),
            tool_calls: vec![ToolCall {
                id: "pie_ctx_0".to_string(),
                name: "project_index".to_string(),
                arguments: r#"{"kind":"summary"}"#.to_string(),
            }],
        };
        let pie_result = Message {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: "pie_ctx_0".to_string(),
                content: "# Project index\n## Clusters\n- tui (8 files)".to_string(),
            }]),
            tool_calls: vec![],
        };
        let task = Message {
            role: "user".to_string(),
            content: MessageContent::Text("Fix the bug".to_string()),
            tool_calls: vec![],
        };
        // Pad with several assistant/tool turn pairs so trim fires
        let mut messages = vec![pie_assistant, pie_result, task];
        for _ in 0..6 {
            messages.push(Message {
                role: "assistant".to_string(),
                content: MessageContent::Text("thinking...".repeat(50)),
                tool_calls: vec![],
            });
            messages.push(Message {
                role: "user".to_string(),
                content: MessageContent::Text("tool result".repeat(50)),
                tool_calls: vec![],
            });
        }

        budget.enforce(&mut messages, 0);

        // PIE pair must survive
        assert_eq!(messages[0].role, "assistant", "PIE assistant must be at index 0");
        assert!(
            has_tool_call_id(&messages[0].tool_calls, "pie_ctx_0"),
            "PIE assistant must retain tool call id"
        );
        assert_eq!(messages[1].role, "user", "PIE tool result must be at index 1");

        // Real task must survive
        let task_present = messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t == "Fix the bug")
        });
        assert!(task_present, "User task must not be dropped");
    }
}


