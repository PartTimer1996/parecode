# Forge

A terminal coding agent built for token efficiency and local model reliability.

Forge completes coding tasks using the tools you'd use yourself — read, edit, search, bash — with a TUI that shows exactly what's happening. It works with any OpenAI-compatible endpoint: Ollama, Anthropic, OpenAI, OpenRouter.

---

## Why Forge

Most coding agents fail on local models because they weren't designed for them. They bloat context with full file reads, accumulate 40k+ tokens of history before the first edit, and lose track of the plan by step three.

Forge is built the other way around:

- **Proactive token budget** — context is compressed before every API call, not reactively when the window fills. A task that costs 40k tokens in other tools costs 4k–12k in Forge.
- **Plan/execute isolation** — each plan step runs as a fresh agent call with only the relevant files loaded. The scaffold owns state. The model only ever sees one bounded step.
- **First-class local model support** — lean hand-written tool schemas, no Zod, no framework overhead. Tested on Qwen3 14B via Ollama.
- **MCP client** — any MCP server (Brave Search, fetch, GitHub, Postgres, etc.) connects with a config entry. Tools appear alongside native tools transparently.
- **Planner/executor split** — use Opus to plan, Haiku to execute. Planning costs ~1k tokens; execution costs 10–40k. The split pays for itself on large tasks.

---

## Install

```bash
git clone https://github.com/your-org/forge
cd forge
cargo build --release
cp target/release/forge ~/.local/bin/
```

Requires Rust 1.75+ and an accessible model endpoint (Ollama running locally is the zero-config path).

---

## Quick start

```bash
# First run — create config file
forge --init

# Single-shot task
forge "remove all console.log calls from src/"

# TUI (interactive, recommended)
forge
```

In the TUI, type your task and press Enter. Use `@` to attach files, `Ctrl+P` for commands.

---

## Plan mode

For multi-step tasks, use `/plan`:

```
/plan "add JWT authentication to the Express API"
```

Forge generates a structured plan, shows it inline for review, then executes each step as a fresh, isolated agent run. The plan is also written to `.forge/plan.md` — open it in your editor while it runs.

Review controls: `↑↓` navigate · `a` approve step · `e` annotate · `Enter` run · `Esc` cancel

---

## Configuration

Config file: `~/.config/forge/config.toml`

```toml
default_profile = "local"

[profiles.local]
endpoint      = "http://localhost:11434"
model         = "qwen3:14b"
context_tokens = 32768
```

Switch profiles at runtime: `forge --profile claude "task"` or `/profile claude` in the TUI.

Full reference: [CONFIG.md](CONFIG.md)

---

## MCP servers (web search, fetch, and more)

Add any MCP server to a profile and its tools become available to the model:

```toml
# Brave Search — free tier at brave.com/search/api
[[profiles.local.mcp_servers]]
name    = "brave"
command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]
[profiles.local.mcp_servers.env]
BRAVE_API_KEY = "BSA..."

# HTTP fetch + HTML→text, no key needed
[[profiles.local.mcp_servers]]
name    = "fetch"
command = ["uvx", "mcp-server-fetch"]
```

See [CONFIG.md](CONFIG.md) for the full MCP reference including GitHub, Postgres, and filesystem servers.

---

## Planner/executor split

Use a powerful model to plan and a fast model to execute:

```toml
[profiles.claude-split]
endpoint      = "https://api.anthropic.com/v1"
model         = "claude-haiku-4-5-20251001"   # executes each step
planner_model = "claude-opus-4-6"             # generates the plan
context_tokens = 200000
api_key       = "sk-ant-..."
```

---

## TUI key reference

| Key | Action |
|---|---|
| `Enter` | Submit task |
| `@` | Attach file (fuzzy picker) |
| `Ctrl+P` | Command palette |
| `Ctrl+H` | Session history browser |
| `Ctrl+C` | Cancel running agent |
| `Esc` | Close overlay |

**Slash commands** (type in input or via `Ctrl+P`):

| Command | Description |
|---|---|
| `/plan "task"` | Generate and review a multi-step plan |
| `/profile <name>` | Switch profile for this session |
| `/new` | Start a fresh session |
| `/resume [n]` | Resume a previous session |
| `/rollback [n]` | Roll back N turns |
| `/clear` | Clear display |
| `/ts` | Toggle timestamps |
| `/quit` | Exit |

---

## Project conventions

Create `.forge/conventions.md` (or `AGENTS.md` / `CLAUDE.md`) in your project root. Forge loads it automatically and appends it to the system prompt:

```markdown
This is a TypeScript project using Bun, not Node.
- Use `bun` to run scripts
- Tests live in src/__tests__/
- Prefer explicit types over inference
```

---

## Architecture

See [PLAN.md](PLAN.md) for the full implementation plan, design rationale, and comparison against OpenCode.

Key files:

| File | Purpose |
|---|---|
| `src/agent.rs` | Agent loop, build check, project map |
| `src/budget.rs` | Proactive token budget, loop detection |
| `src/plan.rs` | Plan generation, step execution, carry-forward summaries |
| `src/mcp.rs` | MCP client — JSON-RPC, tool discovery, dispatch |
| `src/sessions.rs` | Session persistence, context injection |
| `src/telemetry.rs` | Per-session stats, `.forge/telemetry.jsonl` |
| `src/history.rs` | Tool output compression |
| `src/cache.rs` | File read cache |
| `src/tools/` | Native tools: read, write, edit, bash, search, list, recall |
| `src/tui/` | Ratatui TUI — event loop, rendering |

---

## Telemetry

Forge records per-task stats to `.forge/telemetry.jsonl` — token spend, tool calls, compression ratio, model. The stats bar in the TUI shows session totals in real time:

```
∑ 4 tasks  18.2ktok  avg 4.5k/task  22 tool calls  36% compressed  peak 48%
```

No data leaves your machine.
