# Forge Configuration Reference

Config file location: `~/.config/forge/config.toml`
Run `forge --init` to create it with commented examples.

---

## Quick start

```toml
default_profile = "local"

[profiles.local]
endpoint      = "http://localhost:11434"
model         = "qwen3:14b"
context_tokens = 32768
```

Run with a specific profile: `forge --profile claude "your task"`

---

## Profile fields

| Field | Required | Description |
|---|---|---|
| `endpoint` | Yes | OpenAI-compatible API base URL |
| `model` | Yes | Model identifier sent to the endpoint |
| `context_tokens` | No | Context window size for budget enforcement (default: 32768) |
| `api_key` | No | Bearer token — sent as `Authorization: Bearer <key>` |
| `planner_model` | No | Separate model for `/plan` generation (see below) |
| `mcp_servers` | No | List of MCP server processes to spawn (see below) |

---

## Endpoints

### Ollama (local)
```toml
[profiles.local]
endpoint      = "http://localhost:11434"
model         = "qwen3:14b"
context_tokens = 32768
```
No API key needed. Works fully offline.

Recommended models: `qwen3:14b`, `qwen2.5-coder:14b`, `deepseek-coder-v2:16b`

### Anthropic Claude
```toml
[profiles.claude]
endpoint       = "https://api.anthropic.com/v1"
model          = "claude-sonnet-4-6"
context_tokens = 200000
api_key        = "sk-ant-..."
```

### OpenAI
```toml
[profiles.openai]
endpoint       = "https://api.openai.com/v1"
model          = "gpt-4o"
context_tokens = 128000
api_key        = "sk-..."
```

### OpenRouter (any model via one endpoint)
```toml
[profiles.openrouter]
endpoint       = "https://openrouter.ai/api/v1"
model          = "qwen/qwen-2.5-coder-32b-instruct"
context_tokens = 32768
api_key        = "sk-or-..."
```

---

## Planner/executor split (`planner_model`)

Use a powerful model for planning and a fast/cheap model for executing each step.
Planning costs ~1–2k tokens. Execution costs 10–40k. The split pays for itself.

```toml
[profiles.claude-split]
endpoint       = "https://api.anthropic.com/v1"
model          = "claude-haiku-4-5-20251001"   # executor — fast, cheap
planner_model  = "claude-opus-4-6"             # planner — slow, smart
context_tokens = 200000
api_key        = "sk-ant-..."
```

When `planner_model` is set, the TUI shows which model is thinking:
```
⟳ planning via claude-opus-4-6: add authentication to the API
```

If `planner_model` is not set, `model` is used for both planning and execution.

---

## MCP servers

MCP (Model Context Protocol) servers give the model extra tools — web search, HTTP fetch,
filesystem access, GitHub, databases, and anything else the MCP ecosystem provides.

Tools appear as `<server_name>.<tool_name>` in the model's tool list.
The model calls them naturally alongside native tools (`read_file`, `bash`, etc.).

```toml
[[profiles.local.mcp_servers]]
name    = "brave"
command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]

[profiles.local.mcp_servers.env]
BRAVE_API_KEY = "BSA..."
```

### Fields

| Field | Required | Description |
|---|---|---|
| `name` | Yes | Server identifier — prefixes all tool names (`brave.brave_web_search`) |
| `command` | Yes | Command + args to spawn the server process |
| `env` | No | Environment variables injected into the server process |

Multiple servers per profile are supported:

```toml
# Web search
[[profiles.local.mcp_servers]]
name    = "brave"
command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]
[profiles.local.mcp_servers.env]
BRAVE_API_KEY = "BSA..."

# HTTP fetch + HTML→text (no API key)
[[profiles.local.mcp_servers]]
name    = "fetch"
command = ["uvx", "mcp-server-fetch"]

# Filesystem access beyond cwd
[[profiles.local.mcp_servers]]
name    = "fs"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/home/user/projects"]
```

### Common MCP servers

| Server | Install | Notes |
|---|---|---|
| Brave Search | `npx -y @modelcontextprotocol/server-brave-search` | Free tier: 2k queries/month at brave.com/search/api |
| Fetch | `uvx mcp-server-fetch` | HTTP fetch + HTML→text, no key needed |
| Filesystem | `npx -y @modelcontextprotocol/server-filesystem <path>` | Read/write outside cwd |
| GitHub | `npx -y @modelcontextprotocol/server-github` | Issues, PRs, repos |
| Postgres | `npx -y @modelcontextprotocol/server-postgres <conn-string>` | Query databases |

Any MCP-compatible server works. Find more at [modelcontextprotocol.io](https://modelcontextprotocol.io).

---

## Multiple profiles + switching

```toml
default_profile = "local"   # used when no --profile flag given

[profiles.local]
endpoint = "http://localhost:11434"
model    = "qwen3:14b"
context_tokens = 32768

[profiles.fast]
endpoint = "http://localhost:11434"
model    = "qwen3:8b"
context_tokens = 32768

[profiles.claude]
endpoint = "https://api.anthropic.com/v1"
model    = "claude-sonnet-4-6"
context_tokens = 200000
api_key  = "sk-ant-..."
```

Switch at runtime:
- CLI: `forge --profile claude "task"`
- TUI: `/profile claude` or `Ctrl+P` → type profile name

---

## Context window sizing (`context_tokens`)

Set this to match your model's actual context window.
Forge uses it for proactive budget enforcement — compressing history before the window fills,
not reactively after.

| Model | Recommended `context_tokens` |
|---|---|
| Qwen3 14B (Ollama) | `32768` |
| Llama 3.1 8B | `8192` |
| DeepSeek Coder V2 | `32768` |
| Claude Sonnet/Haiku | `200000` |
| GPT-4o | `128000` |

Setting it too high wastes nothing — the budget enforcer only acts when needed.
Setting it too low triggers premature compression.

---

## Project conventions

Forge auto-loads project-specific instructions from (in order):
1. `AGENTS.md` in the current directory
2. `CLAUDE.md` in the current directory
3. `.forge/conventions.md`

These are appended to the system prompt. Use them to tell the model about your stack,
style rules, or things to avoid. Example `.forge/conventions.md`:

```markdown
This is a TypeScript project using Bun (not Node).
- Always use `bun` to run scripts, not `npm` or `node`
- Prefer `const` over `let`
- Tests live in `src/__tests__/`
```

---

## Plan output

When you run `/plan "task"`, Forge writes two files:

- `.forge/plans/{timestamp}-plan.json` — machine-readable, used for resume/reload
- `.forge/plan.md` — human-readable markdown, open in your editor while the plan runs

The markdown version is overwritten each time a new plan is generated.
