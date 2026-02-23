/// MCP (Model Context Protocol) client.
///
/// Spawns MCP server processes, performs the JSON-RPC 2.0 handshake,
/// discovers tools, and dispatches calls. Each server runs as a child
/// process communicating over stdin/stdout.
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

// ── JSON-RPC types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct Request {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[allow(dead_code)]
    id: Option<Value>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
    #[allow(dead_code)]
    code: Option<i64>,
}

// ── MCP tool descriptor (as returned by tools/list) ───────────────────────────

#[derive(Debug, Clone)]
pub struct McpTool {
    /// Qualified name: "<server_name>.<tool_name>" — e.g. "brave.brave_web_search"
    pub qualified_name: String,
    /// Original tool name used when calling the server
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
    /// Which server owns this tool
    pub server_name: String,
}

// ── Per-server state ───────────────────────────────────────────────────────────

struct ServerConn {
    name: String,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    #[allow(dead_code)]
    child: Child,
    tools: Vec<McpTool>,
    next_id: AtomicU64,
}

impl ServerConn {
    async fn send_request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("MCP stdin write failed")?;
        self.stdin.flush().await?;

        // Read lines until we get the response for our id (skip notifications)
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self.stdout.read_line(&mut buf).await?;
            if n == 0 {
                return Err(anyhow!("MCP server '{}' closed stdout", self.name));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: Response = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => continue, // skip malformed / notification lines
            };
            // Match our request id
            let resp_id = resp.id.as_ref().and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
            if resp_id != id {
                continue; // different id — keep reading
            }
            if let Some(err) = resp.error {
                return Err(anyhow!("MCP error from '{}': {}", self.name, err.message));
            }
            return resp.result.ok_or_else(|| anyhow!("MCP response had no result"));
        }
    }

    async fn call_tool(&mut self, tool_name: &str, arguments: Value) -> Result<String> {
        let result = self
            .send_request(
                "tools/call",
                Some(json!({ "name": tool_name, "arguments": arguments })),
            )
            .await?;

        // MCP tools/call result: { content: [ {type: "text", text: "..."} ] }
        extract_text_content(&result)
    }
}

fn extract_text_content(result: &Value) -> Result<String> {
    let content = result.get("content").and_then(|c| c.as_array());
    if let Some(parts) = content {
        let mut out = String::new();
        for part in parts {
            match part.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
                Some("image") => {
                    out.push_str("[image content not displayed]");
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    // Fallback: just dump the result
    Ok(serde_json::to_string_pretty(result)?)
}

// ── McpClient (public) ────────────────────────────────────────────────────────

pub struct McpClient {
    servers: Mutex<HashMap<String, ServerConn>>,
}

impl McpClient {
    /// Spawn all configured MCP servers, perform initialization, discover tools.
    /// Silently skips servers that fail to start (logs to stderr).
    pub async fn new(server_configs: &[crate::config::McpServerConfig]) -> Arc<Self> {
        let mut servers = HashMap::new();
        for cfg in server_configs {
            match spawn_and_init(cfg).await {
                Ok(conn) => {
                    eprintln!("[mcp] connected: {} ({} tools)", cfg.name, conn.tools.len());
                    servers.insert(cfg.name.clone(), conn);
                }
                Err(e) => {
                    eprintln!("[mcp] failed to start '{}': {e}", cfg.name);
                }
            }
        }
        Arc::new(Self {
            servers: Mutex::new(servers),
        })
    }

    /// All tools discovered across all running servers.
    pub async fn all_tools(&self) -> Vec<McpTool> {
        let servers = self.servers.lock().await;
        servers.values().flat_map(|s| s.tools.clone()).collect()
    }

    /// Call a tool by its qualified name ("server.tool_name").
    pub async fn call(&self, qualified_name: &str, arguments: Value) -> Result<String> {
        let (server_name, tool_name) = qualified_name
            .split_once('.')
            .ok_or_else(|| anyhow!("Invalid MCP tool name: {qualified_name}"))?;
        let mut servers = self.servers.lock().await;
        let conn = servers
            .get_mut(server_name)
            .ok_or_else(|| anyhow!("No MCP server named '{server_name}'"))?;
        conn.call_tool(tool_name, arguments).await
    }
}

// ── Server spawn + initialization ─────────────────────────────────────────────

async fn spawn_and_init(cfg: &crate::config::McpServerConfig) -> Result<ServerConn> {
    use tokio::process::Command;

    if cfg.command.is_empty() {
        return Err(anyhow!("Empty command for MCP server '{}'", cfg.name));
    }

    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..]);

    // Inject env vars from config
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()); // pass server stderr to our stderr for debugging

    let mut child = cmd.spawn().context(format!(
        "Failed to spawn MCP server '{}': {:?}",
        cfg.name, cfg.command
    ))?;

    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());

    let mut conn = ServerConn {
        name: cfg.name.clone(),
        stdin,
        stdout,
        child,
        tools: Vec::new(),
        next_id: AtomicU64::new(1),
    };

    // MCP initialization handshake
    let _init_result = conn
        .send_request(
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "clientInfo": { "name": "parecode", "version": "0.1.0" }
            })),
        )
        .await
        .context("MCP initialize failed")?;

    // Notify server that init is complete
    // (initialized is a notification — no id, no response expected)
    let notif = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
    conn.stdin.write_all(notif.as_bytes()).await?;
    conn.stdin.flush().await?;

    // Discover tools
    let tools_result = conn
        .send_request("tools/list", None)
        .await
        .context("MCP tools/list failed")?;

    conn.tools = parse_tools(&cfg.name, &tools_result);

    Ok(conn)
}

fn parse_tools(server_name: &str, result: &Value) -> Vec<McpTool> {
    let Some(tools_arr) = result.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    tools_arr
        .iter()
        .filter_map(|t| {
            let tool_name = t.get("name")?.as_str()?.to_string();
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

            // Qualify the name so it's unique across servers
            let qualified_name = format!("{server_name}.{tool_name}");

            Some(McpTool {
                qualified_name,
                tool_name,
                description,
                input_schema,
                server_name: server_name.to_string(),
            })
        })
        .collect()
}
