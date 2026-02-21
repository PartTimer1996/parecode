/// Proactive token budget enforcement.
///
/// Unlike OpenCode (which reacts at 90% capacity with an LLM summarisation
/// call), Forge enforces budgets *before* each API call using deterministic
/// compression — no model calls, no token cost to save tokens.
///
/// Strategy when over budget:
///   1. Compress oldest tool results further (already summarised; now inline-only)
///   2. If still over: one-sentence summaries of oldest conversation turns
///   3. Hard floor: never drop the system prompt or the original user task
use crate::client::{ContentPart, Message, MessageContent};

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
        // Never drop: index 0 (first user message = the task)
        // Never drop: the last 4 messages (last 2 turns)
        let protected_tail = 4usize;

        if messages.len() <= protected_tail + 1 {
            return;
        }

        // Find the first non-user message we can drop (index 1 onwards, not in tail)
        let drop_before = messages.len() - protected_tail;
        let mut drop_idx = None;

        for i in 1..drop_before {
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
        // Fingerprint: tool name + first 200 chars of args (cheap, good enough)
        let fp = format!("{tool_name}::{}", &args[..args.len().min(200)]);

        // Keep last 5
        self.recent.push((tool_name.to_string(), fp.clone()));
        if self.recent.len() > 5 {
            self.recent.remove(0);
        }

        // Loop = same fingerprint appears twice in recent history
        let count = self.recent.iter().filter(|(_, f)| f == &fp).count();
        count >= 2
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }
}

