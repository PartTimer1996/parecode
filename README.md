# PareCode

**Works with any model. Uses 5–10x fewer tokens. Installs in 30 seconds.**

A terminal coding agent that doesn't waste your money or lock you in. Use Claude, GPT, Qwen, Ollama, or anything OpenAI-compatible — switch providers with one line, not a migration. PareCode completes coding tasks using the tools you'd use yourself — read, edit, search, bash — with a full TUI that shows exactly what's happening and what it's costing you.

---

## Install

One command. No Node, no Python, no runtime. Single static binary.

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/PartTimer1996/parecode/releases/latest/download/parecode-installer.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://github.com/PartTimer1996/parecode/releases/latest/download/parecode-installer.ps1 | iex
```

```bash
# Or via cargo
cargo install parecode
```

<details>
<summary>Build from source</summary>

```bash
git clone https://github.com/PartTimer1996/parecode
cd parecode
cargo build --release
cp target/release/parecode ~/.local/bin/
```
Requires Rust 1.90+.
</details>

<details>
<summary>Keeping up to date</summary>

```bash
# Self-update to the latest release
parecode --update
```

PareCode checks for updates in the background and notifies you in the TUI when a new version is available.
</details>

| Tool | Install | Requires |
|---|---|---|
| **PareCode** | `curl \| sh` | Nothing |
| Claude Code | `npm install -g @anthropic-ai/claude-code` | Node.js, Anthropic account |
| OpenCode | `npm install -g opencode` | Node.js |
| Aider | `pip install aider-chat` | Python |

---

## Get started

```bash
parecode
```

No config file to write by hand. On first launch, a setup wizard runs in the terminal — pick your provider, confirm the defaults, and you're coding. The whole thing takes about 15 seconds:

1. **Cloud API** (Anthropic, OpenAI, OpenRouter) — if you have `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` set, the wizard picks it up automatically. Confirm and go.
2. **Local model** (Ollama) — the wizard probes `localhost:11434`, finds your installed models, and lets you pick one. No `/v1` suffix to figure out, no manual endpoint config.
3. **Custom endpoint** — vLLM, LM Studio, text-gen-webui, anything OpenAI-compatible. Enter the URL and model name.

Don't waste time reading documentation, get installed and up and running in minutes!

```bash
# Or go direct with a task
parecode "remove all console.log calls from src/"
```

In the TUI: type your task, press Enter. `@` to attach files. `Ctrl+P` for commands. `/plan "task"` for multi-step work.

---

## Why PareCode

Coding agents are expensive, complicated, and want to own you. Claude Code locks you to Anthropic. OpenCode needs Node.js and a weekend of configuration. Aider needs Python, git setup, and patience. All of them burn through tokens like they're free.

PareCode takes a different approach:

- **5–10x fewer tokens** — context is managed proactively before every API call, not reactively when the window fills. A task that costs $0.40 in other tools costs $0.04 in PareCode. You see exactly what you're spending, in real time, every session.
- **Any model, any provider** — OpenRouter, Anthropic, OpenAI, Ollama, any OpenAI-compatible endpoint. Switch between Claude and GPT and local models with `/profile`. Your workflow doesn't change.
- **Plan/execute isolation** — each plan step runs as a fresh agent call with only the relevant files loaded. The scaffold owns state. The model only ever sees one bounded step. This is why it works on small models where other agents fall apart.
- **Full transparency** — live token count, cost estimate before plan execution, telemetry stats bar. No hidden API calls. No surprise bills.

### What makes it efficient

Other agents read entire files, accumulate 40k+ tokens of conversation history, and then reactively compress when the context window fills — costing even more tokens. PareCode does the opposite:

| | Other agents | PareCode |
|---|---|---|
| Context management | React at 90% capacity | Enforce budget before every call |
| File reads | Full file, up to 50KB | Smart excerpt, 150 lines max, symbol index |
| Edit correctness | Search/replace, hope it matches | Hash-anchored lines, stale-edit detection |
| Multi-step tasks | Whole conversation in context | Fresh context per step, scaffold carries state |
| Error handling | Model re-reads file to see error | Hook output injected inline, model self-corrects |
| Loop detection | 3 identical calls | 2 calls, cached result injected |

---

## Features

### Plan mode

For multi-step tasks, use `/plan`:

```
/plan "add JWT authentication to the Express API"
```

PareCode generates a structured plan, shows estimated token cost and API cost, then lets you review each step before execution. Each step runs as a fresh, isolated agent call. The plan is also written to `.parecode/plan.md` — open it in your editor while it runs.

Review controls: `↑↓` navigate · `a` approve step · `e` annotate · `Enter` run · `Esc` cancel

### Use any provider

Set up as many profiles as you want and switch between them mid-session.

```toml
# ~/.config/parecode/config.toml
default_profile = "openrouter"

[profiles.openrouter]
endpoint       = "https://openrouter.ai/api/v1"
model          = "qwen/qwen-2.5-coder-32b-instruct"
context_tokens = 32768
api_key        = "sk-or-..."

[profiles.claude]
endpoint       = "https://api.anthropic.com/v1"
model          = "claude-sonnet-4-6"
context_tokens = 200000
api_key        = "sk-ant-..."

[profiles.local]
endpoint       = "http://localhost:11434"
model          = "qwen3:14b"
context_tokens = 32768
```

Switch at runtime: `parecode --profile claude "task"` or `/profile claude` in the TUI. Your tools, sessions, hooks — everything stays the same. Only the model changes.

Full reference: [CONFIG.md](CONFIG.md)

### Planner/executor split

Use a powerful model to plan and a fast model to execute:

```toml
[profiles.claude-split]
endpoint      = "https://api.anthropic.com/v1"
model         = "claude-haiku-4-5-20251001"   # executes each step
planner_model = "claude-opus-4-6"             # generates the plan
context_tokens = 200000
api_key       = "sk-ant-..."
```

Planning is ~1k tokens; execution is 10–40k. The split pays for itself.

### MCP servers

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

### Git integration

PareCode auto-checkpoints your working tree before every task. If something goes wrong:

- **`/undo`** — interactive checkpoint picker, reverts with `git reset --hard`
- **`d`** — full-screen syntax-highlighted diff of what the agent changed
- **Auto-commit** (opt-in) — `auto_commit = true` in profile config

Git status is injected into the system prompt so the model knows what's dirty without a tool call.

### Hooks system

Auto-detected from your project. If PareCode sees `Cargo.toml`, it runs `cargo check -q` after every edit. TypeScript project? `tsc --noEmit`. Error output is injected directly into the model's tool result — the model self-corrects immediately without re-reading the file.

```toml
[profiles.local.hooks]
on_edit      = ["cargo check -q"]
on_task_done = ["cargo test -q 2>&1 | tail -5"]
```

### Session persistence

Resume where you left off, roll back turns, branch conversations. Sessions are stored as JSONL in `~/.local/share/parecode/sessions/` and auto-resumed for the matching project directory.

### Shell completions

```bash
parecode --completions zsh > ~/.zfunc/_parecode
parecode --completions bash > ~/.bash_completion.d/parecode
parecode --completions fish > ~/.config/fish/completions/parecode.fish
```

---

## TUI reference

| Key | Action |
|---|---|
| `Enter` | Submit task |
| `@` | Attach file (fuzzy picker) |
| `Ctrl+P` | Command palette |
| `Ctrl+H` | Session history browser |
| `Ctrl+B` | Toggle session sidebar |
| `Ctrl+C` | Cancel running agent |
| `1`–`5` | Switch tabs (Chat, Config, Git, Stats, Plan) |
| `d` | Show diff of agent changes |
| `Esc` | Close overlay |

**Slash commands** (type in input or via `Ctrl+P`):

| Command | Description |
|---|---|
| `/plan "task"` | Generate and review a multi-step plan |
| `/profile <name>` | Switch profile for this session |
| `/quick "task"` | Single-shot, minimal-context mode |
| `/new` | Start a fresh session |
| `/resume [n]` | Resume a previous session |
| `/rollback [n]` | Roll back N turns |
| `/undo` | Revert to last git checkpoint |
| `/diff` | Show git diff |
| `/hooks on\|off` | Toggle hooks for this session |
| `/init` | Auto-detect project conventions |
| `/clear` | Clear display |
| `/ts` | Toggle timestamps |
| `/quit` | Exit |

---

## Project conventions

Create `.parecode/conventions.md` (or `AGENTS.md` / `CLAUDE.md`) in your project root. PareCode loads it automatically and appends it to the system prompt:

```markdown
This is a TypeScript project using Bun, not Node.
- Use `bun` to run scripts
- Tests live in src/__tests__/
- Prefer explicit types over inference
```

Or run `/init` — PareCode scans your project and generates conventions automatically from your `README.md`, package manifest, linter config, and test structure.

---

## Know what you're spending

The stats bar in the TUI shows session totals in real time:

```
∑ 4 tasks  18.2ktok  avg 4.5k/task  22 tool calls  36% compressed  peak 48%
```

Plan mode shows estimated token cost and API cost **before** execution starts. No surprises, no $638 invoices.

Per-task stats are recorded to `.parecode/telemetry.jsonl` — token spend, tool calls, compression ratio, model. All telemetry is local. No data leaves your machine. Ever.

---

## Architecture

See [PLAN.md](PLAN.md) for the full implementation plan, design rationale, and comparison against OpenCode.

| File | Purpose |
|---|---|
| `src/agent.rs` | Agent loop, build check, project map |
| `src/budget.rs` | Proactive token budget, loop detection |
| `src/plan.rs` | Plan generation, step execution, carry-forward summaries |
| `src/mcp.rs` | MCP client — JSON-RPC, tool discovery, dispatch |
| `src/sessions.rs` | Session persistence, context injection |
| `src/git.rs` | Git integration — checkpoint, undo, diff |
| `src/hooks.rs` | Auto-detected hooks, on_edit output injection |
| `src/telemetry.rs` | Per-session stats, `.parecode/telemetry.jsonl` |
| `src/history.rs` | Tool output compression |
| `src/cache.rs` | File read cache |
| `src/tools/` | Native tools: read, write, edit, patch, bash, search, list, recall |
| `src/tui/` | Ratatui TUI — tabs, event loop, rendering, overlays |

---

## License

MIT
