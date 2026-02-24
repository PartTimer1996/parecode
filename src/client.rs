use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text(String::new())
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    _finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
    /// Reasoning/thinking tokens from models that return them as a separate field
    /// (DeepSeek-R1, Qwen3 with thinking enabled, etc.)
    reasoning_content: Option<String>,
    /// Alternative reasoning field used by OpenRouter / StepFun / other providers
    reasoning: Option<String>,
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

        let url = self.endpoint.clone();

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
        let mut reasoning_buf = String::new();
        // Index → accumulator
        let mut pending: Vec<PendingToolCall> = Vec::new();
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut leftover = String::new();
        // Track whether we're mid-reasoning-block (for models that use reasoning_content field)
        let mut reasoning_open = false;

        // Debug log — raw stream to /tmp/parecode-stream.log for diagnosing model output
        let mut debug_log: Option<std::fs::File> = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("/tmp/parecode-stream.log")
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

                        // Reasoning tokens — models use different field names:
                        //   reasoning_content: DeepSeek-R1, Qwen3
                        //   reasoning: OpenRouter, StepFun, others
                        // Wrap in <think> tags so the TUI renders them in the thinking panel.
                        let rc = delta.reasoning_content.or(delta.reasoning);
                        if let Some(rc) = rc {
                            if !rc.is_empty() {
                                if !reasoning_open {
                                    on_text("<think>");
                                    reasoning_open = true;
                                }
                                on_text(&rc);
                                reasoning_buf.push_str(&rc);
                            }
                        } else if reasoning_open {
                            // reasoning field stopped arriving — close the tag
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

        // If the model sent everything as reasoning tokens (content was always empty),
        // use the reasoning buffer as the response text. This happens with
        // OpenRouter/StepFun models that put the entire response in `reasoning`.
        let final_text = if text_buf.is_empty() && !reasoning_buf.is_empty() {
            reasoning_buf
        } else {
            text_buf
        };

        Ok(ModelResponse {
            text: final_text,
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
                if !msg.tool_calls.is_empty() {
                    // Assistant message that triggered tool calls — include tool_calls array
                    let tc_json: Vec<Value> = msg.tool_calls.iter().map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments
                            }
                        })
                    }).collect();
                    out.push(serde_json::json!({
                        "role": msg.role,
                        "content": text,
                        "tool_calls": tc_json
                    }));
                } else {
                    out.push(serde_json::json!({
                        "role": msg.role,
                        "content": text
                    }));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_part_serialize() {
        let part = ContentPart::Text { text: "hello".to_string() };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_model_response() {
        let response = ModelResponse {
            text: "test response".to_string(),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 200,
        };
        assert_eq!(response.text, "test response");
        assert_eq!(response.input_tokens, 100);
        assert_eq!(response.output_tokens, 200);
    }

    #[test]
    fn test_pending_tool_call_default() {
        let pending = PendingToolCall::default();
        assert!(pending.id.is_empty());
        assert!(pending.name.is_empty());
        assert!(pending.arguments.is_empty());
    }

    #[test]
    fn test_message_content_from_str() {
        let content: MessageContent = "hello".into();
        match content {
            MessageContent::Text(text) => assert_eq!(text, "hello"),
            MessageContent::Parts(_) => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_message_content_from_string() {
        let content: MessageContent = "world".to_string().into();
        match content {
            MessageContent::Text(text) => assert_eq!(text, "world"),
            MessageContent::Parts(_) => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_message_serialize() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Text("test message".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"test message\""));
    }

    #[test]
    fn test_message_deserialize() {
        let json = r#"{"role":"assistant","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, "assistant");
        match msg.content {
            MessageContent::Text(text) => assert_eq!(text, "hello"),
            MessageContent::Parts(_) => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_content_part_text_serialize() {
        let part = ContentPart::Text { text: "hello".to_string() };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"hello\""));
    }

    #[test]
    fn test_content_part_tool_result_serialize() {
        let part = ContentPart::ToolResult {
            tool_use_id: "call_123".to_string(),
            content: "result data".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        assert!(json.contains("\"tool_use_id\":\"call_123\""));
        assert!(json.contains("\"content\":\"result data\""));
    }

    #[test]
    fn test_content_part_deserialize_text() {
        let json = r#"{"type":"text","text":"hello world"}"#;
        let part: ContentPart = serde_json::from_str(json).unwrap();
        match part {
            ContentPart::Text { text } => assert_eq!(text, "hello world"),
            ContentPart::ToolResult { .. } => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_content_part_deserialize_tool_result() {
        let json = r#"{"type":"tool_result","tool_use_id":"call_abc","content":"42"}"#;
        let part: ContentPart = serde_json::from_str(json).unwrap();
        match part {
            ContentPart::ToolResult { tool_use_id, content } => {
                assert_eq!(tool_use_id, "call_abc");
                assert_eq!(content, "42");
            }
            ContentPart::Text { .. } => panic!("Expected ToolResult variant"),
        }
    }

    #[test]
    fn test_tool_serialize() {
        let tool = Tool {
            name: "get_weather".to_string(),
            description: "Get weather for a location".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string"}
                }
            }),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"get_weather\""));
        assert!(json.contains("\"description\":\"Get weather for a location\""));
    }

    #[test]
    fn test_build_messages_empty_system() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("hello".to_string()),
            ..Default::default()
        }];
        let result = build_messages("", &messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"], "hello");
    }

    #[test]
    fn test_build_messages_with_system() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("hello".to_string()),
            ..Default::default()
        }];
        let result = build_messages("You are a helpful assistant", &messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[0]["content"], "You are a helpful assistant");
        assert_eq!(result[1]["role"], "user");
    }

    #[test]
    fn test_build_messages_with_tool_result_parts() {
        let messages = vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: "Calling tool".to_string() },
                ContentPart::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "result".to_string(),
                },
            ]),
            ..Default::default()
        }];
        let result = build_messages("", &messages);
        // Should flatten to 2 messages: assistant text + tool result
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "assistant");
        assert_eq!(result[0]["content"], "Calling tool");
        assert_eq!(result[1]["role"], "tool");
        assert_eq!(result[1]["tool_call_id"], "call_1");
        assert_eq!(result[1]["content"], "result");
    }

    #[test]
    fn test_build_messages_multiple_messages() {
        let messages = vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::Text("first".to_string()),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Text("second".to_string()),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Parts(vec![ContentPart::Text {
                    text: "third".to_string(),
                }]),
                ..Default::default()
            },
        ];
        let result = build_messages("system prompt", &messages);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[2]["role"], "assistant");
        assert_eq!(result[3]["role"], "user");
    }

    #[test]
    fn test_tool_call_serialize() {
        let tool_call = ToolCall {
            id: "call_xyz".to_string(),
            name: "get_weather".to_string(),
            arguments: r#"{"city":"Paris"}"#.to_string(),
        };
        let json = serde_json::to_string(&tool_call).unwrap();
        assert!(json.contains("\"id\":\"call_xyz\""));
        assert!(json.contains("\"name\":\"get_weather\""));
        assert!(json.contains("\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\""));
    }

    #[test]
    fn test_model_response_equality() {
        let response1 = ModelResponse {
            text: "test".to_string(),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 200,
        };
        let response2 = ModelResponse {
            text: "test".to_string(),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 200,
        };
        assert_eq!(response1.text, response2.text);
        assert_eq!(response1.input_tokens, response2.input_tokens);
    }

    #[test]
    fn test_client_new() {
        let client = Client::new(
            "https://api.example.com/v1/chat".to_string(),
            "gpt-4".to_string(),
        );
        assert_eq!(client.endpoint, "https://api.example.com/v1/chat");
        assert_eq!(client.model, "gpt-4");
        assert!(client.api_key.is_none());
    }

    #[test]
    fn test_client_set_api_key() {
        let mut client = Client::new(
            "https://api.example.com/v1/chat".to_string(),
            "gpt-4".to_string(),
        );
        assert!(client.api_key.is_none());
        client.set_api_key("sk-test123".to_string());
        assert_eq!(client.api_key, Some("sk-test123".to_string()));
    }
}
