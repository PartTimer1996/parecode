use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    pub fn as_str(&self) -> &str {
        match self {
            MessageContent::Text(s) => s.as_str(),
            MessageContent::Parts(_) => "",
        }
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ToolResult { tool_use_id: String, content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

// ── Completed tool call (after accumulating deltas) ───────────────────────────

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

// ── Model response after streaming completes ──────────────────────────────────

#[derive(Debug)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ── SSE delta types for accumulation ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    usage: Option<UsageStats>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
    /// Reasoning/thinking tokens from models that return them as a separate field
    /// (DeepSeek-R1, Qwen3 with thinking enabled, etc.)
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageStats {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

// ── In-progress tool call accumulator ────────────────────────────────────────

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct Client {
    http: reqwest::Client,
    pub endpoint: String,
    pub model: String,
    api_key: Option<String>,
}

impl Client {
    pub fn new(endpoint: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint,
            model,
            api_key: None,
        }
    }

    pub fn set_api_key(&mut self, key: String) {
        self.api_key = Some(key);
    }

    /// Stream a chat completion. Calls `on_text` for each text chunk as it arrives.
    /// Returns the complete response once streaming finishes.
    pub async fn chat(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
        on_text: impl Fn(&str),
    ) -> Result<ModelResponse> {
        let mut body = serde_json::json!({
            "model": self.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": build_messages(system, messages),
        });

        if !tools.is_empty() {
            body["tools"] = serde_json::json!(
                tools.iter().map(|t| serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })).collect::<Vec<_>>()
            );
            body["tool_choice"] = serde_json::json!("auto");
        }

        let url = format!("{}/v1/chat/completions", self.endpoint.trim_end_matches('/'));

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error {}: {}", status, text));
        }

        let mut stream = resp.bytes_stream();

        let mut text_buf = String::new();
        // Index → accumulator
        let mut pending: Vec<PendingToolCall> = Vec::new();
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut leftover = String::new();
        // Track whether we're mid-reasoning-block (for models that use reasoning_content field)
        let mut reasoning_open = false;

        // Debug log — raw stream to /tmp/forge-stream.log for diagnosing model output
        let mut debug_log: Option<std::fs::File> = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("/tmp/forge-stream.log")
            .ok();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            let raw = std::str::from_utf8(&bytes).unwrap_or("");

            // SSE may split across chunks; prepend any leftover from last iteration
            let combined = format!("{}{}", leftover, raw);
            leftover.clear();

            for line in combined.lines() {
                let line = line.trim();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                let json_str = match line.strip_prefix("data: ") {
                    Some(s) => s,
                    None => continue,
                };

                // If JSON is incomplete (split mid-chunk), save for next iteration
                let chunk_val: StreamChunk = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(_) => {
                        leftover = line.to_string();
                        continue;
                    }
                };

                if let Some(usage) = chunk_val.usage {
                    input_tokens = usage.prompt_tokens.unwrap_or(0);
                    output_tokens = usage.completion_tokens.unwrap_or(0);
                }

                for choice in chunk_val.choices.unwrap_or_default() {
                    if let Some(delta) = choice.delta {
                        // Debug log: write raw delta JSON so we can see what the model emits
                        if let Some(f) = &mut debug_log {
                            use std::io::Write as _;
                            let _ = writeln!(f, "{json_str}");
                        }

                        // reasoning_content field (DeepSeek-R1, Qwen3 thinking mode, etc.)
                        // Wrap in <think> tags so the agent's splitter routes it correctly
                        if let Some(rc) = delta.reasoning_content {
                            if !rc.is_empty() {
                                if !reasoning_open {
                                    on_text("<think>");
                                    reasoning_open = true;
                                }
                                on_text(&rc);
                            }
                        } else if reasoning_open {
                            // reasoning_content stopped arriving — close the tag
                            on_text("</think>");
                            reasoning_open = false;
                        }

                        // Accumulate text
                        if let Some(text) = delta.content {
                            if reasoning_open {
                                // Some models send content="" alongside last reasoning chunk
                                if !text.is_empty() {
                                    on_text("</think>");
                                    reasoning_open = false;
                                    on_text(&text);
                                    text_buf.push_str(&text);
                                }
                            } else {
                                on_text(&text);
                                text_buf.push_str(&text);
                            }
                        }

                        // Accumulate tool call deltas
                        for tc_delta in delta.tool_calls.unwrap_or_default() {
                            let idx = tc_delta.index;
                            // Grow pending vec if needed
                            while pending.len() <= idx {
                                pending.push(PendingToolCall::default());
                            }
                            let entry = &mut pending[idx];
                            if let Some(id) = tc_delta.id {
                                entry.id = id;
                            }
                            if let Some(func) = tc_delta.function {
                                if let Some(name) = func.name {
                                    entry.name.push_str(&name);
                                }
                                if let Some(args) = func.arguments {
                                    entry.arguments.push_str(&args);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Close any still-open reasoning block
        if reasoning_open {
            on_text("</think>");
        }

        // Write separator to debug log
        if let Some(f) = &mut debug_log {
            use std::io::Write as _;
            let _ = writeln!(f, "---END---");
        }

        let tool_calls = pending
            .into_iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| ToolCall {
                id: p.id,
                name: p.name,
                arguments: p.arguments,
            })
            .collect();

        Ok(ModelResponse {
            text: text_buf,
            tool_calls,
            input_tokens,
            output_tokens,
        })
    }
}

// ── Build the messages array for the API ──────────────────────────────────────

fn build_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();

    if !system.is_empty() {
        out.push(serde_json::json!({
            "role": "system",
            "content": system
        }));
    }

    for msg in messages {
        match &msg.content {
            MessageContent::Text(text) => {
                out.push(serde_json::json!({
                    "role": msg.role,
                    "content": text
                }));
            }
            MessageContent::Parts(parts) => {
                // Flatten parts for OpenAI-compat: tool results become individual messages
                for part in parts {
                    match part {
                        ContentPart::ToolResult { tool_use_id, content } => {
                            out.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content
                            }));
                        }
                        ContentPart::Text { text } => {
                            out.push(serde_json::json!({
                                "role": msg.role,
                                "content": text
                            }));
                        }
                    }
                }
            }
        }
    }

    out
}
