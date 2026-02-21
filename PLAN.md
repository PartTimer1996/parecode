# Forge — Implementation Plan

> Build a Rust CLI coding agent that matches OpenCode's baseline, then beats it on token efficiency and small-model reliability. Hyper-optimised orchestration + smart deterministic programming where a model call would be wasteful.

---

## Market Position

**The core bet:** context efficiency is the hard problem. Features are plumbing. A model drowning in 60k tokens of accumulated history fails. A model given 8k tokens of clean, relevant context succeeds — and on a 14B local model, this is the difference between working and not working.

**Why this wins:**

| Dimension | OpenCode / Cursor / Claude Code | Forge |
|---|---|---|
| Token usage per task | 20k–60k (reactive compression, full file reads) | 3k–12k (proactive, compressed from the start) |
| Local model support | Broken on most OSS backends (Zod schemas, context bloat) | First-class — designed for Qwen3 14B, Ollama |
| Plan/execute isolation | Plans in conversation — model loses thread by step 3 | Each step: fresh context, bounded instruction, scaffold carries state |
| Loop detection | 3 identical calls before intervention | 2 calls — injects cached result immediately |
| Cost | Cloud API required; usage compounds | Works on free local inference; cloud optional |
| Enterprise / IP | Code leaves the building | Self-hosted, air-gapped capable |

**The efficiency story compounds over time.** As local models improve (Qwen4, etc), Forge gets better for free. We're not locked to any provider's pricing decisions. And every token saved is real money: a team of 10 running 50 tasks/day at OpenCode's token rate vs Forge's is hundreds of dollars a month difference.

**What's genuinely novel:**
- Plan/execute separation where the scaffold owns state and the model only sees one bounded step at a time. No other agent does this.
- Tool output compression that is deterministic and immediate, not a reactive LLM call at 90% capacity.
- Per-step file symbol summaries carried forward between steps — the model knows what changed without seeing implementation detail.

---

## Why OpenCode Falls Over (Validated Against Their Codebase)

| Failure | Impact |
|---|---|
| System prompt bloat (one user hit 217,905 tokens) | Entire context consumed before conversation starts |
| Full file reads (up to 50KB per read) | Most content irrelevant, wastes model attention |
| Glob returns 100K+ tokens per call | Known issue, unfixed |
| Tool outputs never compressed mid-session | History balloons; blunt compaction fires at 90% |
| Compaction is reactive LLM call | Costs tokens to save tokens |
| Doom loop detection fires at 3 identical calls | Already wasted 3 tool round-trips |
| Zod schemas break on OSS backends (SGLang, K2.5) | Tools literally don't work on many local models |
| No per-step context isolation | Small models lose the plan by step 3 |
| Hidden cheap-model calls (Haiku) | Unexpected cost accumulation |
| No conversation persistence | Can't resume, roll back, or compare sessions |

---

## ✅ Phase 1 — Match OpenCode — COMPLETE

**`src/client.rs`** — Ollama/OpenAI-compatible HTTP client
- POST to `/v1/chat/completions` with streaming SSE
- Parse streamed tool call deltas into complete tool calls
- `stream_options: {include_usage: true}` for Ollama token counts
- Config: endpoint URL + model from `~/.config/forge/config.toml`

**`src/tools/`** — Core tool set with lean handwritten JSON schemas
- `read_file`, `write_file`, `edit_file`, `bash`, `search`, `list_files`
- All schemas minimal — work correctly on Qwen3 14B, Ollama backends

**`src/agent.rs`** — Agent loop with streaming output

**`src/main.rs`** — CLI via `clap` — `forge "task"`, `--dry-run`, `-v`, `--profile`, `--init`, `--profiles`

---

## ✅ Phase 2 — Easy Wins That Beat OpenCode — COMPLETE

### ✅ 2a. Tool Output Compression (`src/history.rs`)
- `read_file` content kept full in model context (needed for editing)
- Separate `display_summary` (one-liner) shown in TUI sidebar
- Budget enforcer compresses older read results when threshold hit
- On `edit_file` failure: file content injected into error response so model can self-correct without re-reading

### ✅ 2b. File Read Cache (`src/cache.rs`)
- All reads cached; cache-hit returns content instantly with age note
- Invalidated on write/edit

### ✅ 2c. Proactive Token Budget (`src/budget.rs`)
- Enforced before every API call (not reactive at 90%)
- Pass 1: compress older tool results, leave most recent intact
- Pass 2: trim oldest turns (protects index 0 — original task)
- Loop detection fires at 2 identical calls (vs OpenCode's 3)

### ✅ 2d. Smart File Excerpting (`src/tools/read.rs`)
- Max 150 lines by default; explicit `line_range` for full access
- `symbols=true` mode returns function/struct/class index with line numbers — lets model navigate large files without reading them

### ✅ 2e. Lean Tool Schemas
- Handwritten, minimal — no Zod, no extra metadata

### ✅ Additional: Ratatui TUI (`src/tui/`)
- Full alternate-screen TUI with conversation history, status bar, input
- Context % and token count in status bar
- `@` file picker overlay (fuzzy search)
- **Attached files panel** — `@` adds file as a pinned chip above input; content injected as preamble in every agent call; protected from budget eviction; Tab/Del to manage chips
- Ctrl+P command palette (`/cd`, `/profile`, `/profiles`, `/clear`, `/ts`, `/quit`)
- Agent cancellation (Ctrl+C)
- Conventions loading: auto-discovers `AGENTS.md` / `CLAUDE.md` / `.forge/conventions.md`

### Observed results vs OpenCode
- ~2.3k tokens for a file analysis task that cost OpenCode 20k+ tokens
- ~443 tokens for a simple query (OpenCode spikes to 10k immediately)
- Model successfully self-corrects edit_file failures without re-reading
- Attached files prevent the "context forget" that caused OpenCode to loop

---

## ✅ Phase 3 — Multi-Turn Conversation Persistence — COMPLETE

### ✅ 3a. In-session conversation history (`src/sessions.rs`)
- `Vec<ConversationTurn>` in `AppState` accumulates across agent runs
- Each turn: user message, agent response text, tool summary
- Prior context injected as preamble on each new run (8k token cap — ~25% of a 32k window)
- Short reply hint: model told "yes/ok/go ahead" are responses to the previous message

### ✅ 3b. Persistent conversation storage
- JSONL files in `~/.local/share/forge/sessions/{ts}_{basename}.jsonl`
- Auto-resumed on startup for the matching cwd

### ✅ 3c. Session management
- `/sessions`, `/resume [n]`, `/rollback [n]`, `/new` slash commands
- `Ctrl+H` session browser overlay — date, project, turn count, first message preview
- Status bar indicator: `◈ N↩` shows active turn count and resumed state

### ✅ 3d. Rollback
- Active turn pointer — rolling back branches without deleting archived turns

---

## ✅ Phase 4 — Plan/Execute Mode — COMPLETE

**The core architectural differentiator.** Plan is a data structure owned by the scaffold. Each step gets fresh, minimal context. The model only ever sees the current step. The scaffold carries all state.

### ✅ Plan data structure (`src/plan.rs`)
- `Plan { task, steps, current, status, created_at, project }`
- `PlanStep { description, instruction, files, verify, status, tool_budget, user_annotation, completed_summary }`
- `Verification`: None | FileChanged | PatternAbsent | CommandSuccess | BuildSuccess

### ✅ Per-step context isolation
- Fresh `messages` vec per step — zero bleed from previous steps
- Only `step.files` loaded as attached context
- Single bounded instruction to model

### ✅ Step carry-forward summaries
- After each step passes, `summarise_completed_step()` scans modified files deterministically
- Extracts top symbols (fn/struct/class/def) from recently modified files
- Result: `"modified src/auth.rs [validate_token, AuthError]; modified src/handler.rs [handle_request]"`
- Injected into next step's preamble — model knows exact function names without seeing implementation
- Zero model calls, ~5 lines of context per completed step

### ✅ TUI plan review
- `/plan "task"` — generate plan, enter inline review mode
- `↑↓` navigate steps, `e` annotate, `a` approve, `Esc` cancel
- Annotations injected as `"\n\nUser note: {}"` into the step instruction
- All steps must be individually approved before execution begins
- Per-step ✓/✗ shown in conversation history during execution

### ✅ Plan persistence
- Plans saved to `.forge/plans/{timestamp}-plan.json` (JSON, machine-readable)
- Plans written to `.forge/plan.md` (Markdown, human-readable — open in editor while plan runs)
- Failed plans paused at the failing step, resumable

### ✅ Plan UX polish
- Overlay closes immediately on Enter confirm — mode transitions to `PlanRunning` synchronously, no async lag
- Planning message shows which model is thinking when `planner_model` is configured: `⟳ planning via claude-opus-4-6: task`

---

## ✅ Phase 5 — Agent Reliability — COMPLETE

### ✅ 5a. `recall` tool
- Schema: `{ tool_call_id?, tool_name? }` — either works
- Handled before dispatch in `agent.rs` — not recorded in history (prevents recursion)
- `recall_by_name()` fallback for local models that don't echo IDs reliably

### ✅ 5b. Bash timeout (async)
- `tokio::process::Command` + `tokio::time::timeout`
- `execute_tool` is now `async fn`
- `MAX_OUTPUT_LINES` = 200

### ✅ 5c. Smart bash summarisation
- Error-line aware: keeps `error:`, `FAILED`, `panic` lines (up to 20)
- Build check failures pass through history compression unchanged
- Build check success prompts model to verify via search before declaring done

### ✅ 5d. Fuzzy `edit_file` matching
- CRLF → LF → per-line trim() → per-line trim_end() cascade
- Only applies if exactly one candidate found
- On failure: ±15 line context hint instead of full file dump

### ✅ 5e. `write_file` existence guard
- `overwrite: bool` required to replace existing files
- Prevents silent overwrites by local models that don't track what exists

### ✅ 5f. Token counting fix
- `s.chars().count() / 4` — correct for multi-byte Unicode
- Prevents premature compression on non-ASCII codebases

### ✅ 5g. Unicode panic fix
- `format_args_summary` now uses `.chars().take(N).collect()` not `&s[..N]`
- Prevents panic on multi-byte chars in tool arg display (∑, Chinese, emoji)

### ✅ 5h. System prompt hardening
- "Do not ask permission mid-task — make necessary changes and report what you did"
- "For replacement tasks, search to confirm no instances remain before declaring done"
- "Do not re-read files already read this session"
- Auto build-check after every file mutation (`cargo check -q` / `tsc --noEmit`)

---

## ✅ Phase 5i — Sub-agent model split — COMPLETE

`planner_model` config field per profile:
- If set, plan generation uses `planner_model`; step execution uses `model`
- Enables Opus plan + Haiku execute — high reasoning where it counts (planning), cheap tokens where they're plentiful (execution)
- Planning is ~1–2k tokens; execution is 10–40k. The split is economically significant.
- Falls back to `model` if `planner_model` not set — zero behaviour change for existing configs
- See `CONFIG.md` for full examples

---

## ✅ Phase 6a — MCP Client — COMPLETE

Full Model Context Protocol client (`src/mcp.rs`):
- Spawns any MCP server process (Node/Python/binary) configured per-profile
- JSON-RPC 2.0 over stdin/stdout with proper `initialize` / `notifications/initialized` handshake
- Dynamic tool discovery via `tools/list` — tools appear as `<server>.<tool>` (e.g. `brave.brave_web_search`)
- Dispatched transparently alongside native tools — model sees one unified tool list
- Multiple servers per profile, all running concurrently
- Silently skips servers that fail to start (logs to stderr)
- Config in `config.toml` per-profile:
  ```toml
  [[profiles.local.mcp_servers]]
  name    = "brave"
  command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]
  [profiles.local.mcp_servers.env]
  BRAVE_API_KEY = "BSA..."
  ```
- Commented examples in default config: Brave Search, filesystem, fetch (`uvx mcp-server-fetch`)

---

## Phase 6b — Growth & Distribution

These are the next moves that turn Forge from a strong tool into a platform.

### 6b-i. Benchmarking suite
Run on the tasks that caused Qwen3 14B to loop in OpenCode. Record token counts, tool calls, success rate, wall time. Publish results.

| Task | Target |
|---|---|
| `"remove all console.log from src/"` | ≤ 5 tool calls, < 5k tokens |
| `"rename columns → allColumns in data-table.component.ts"` | No re-reads, clean 1-shot |
| `"reorganise SCSS in header.component.scss"` | < 3k tokens |

Model matrix: Qwen3 14B (Ollama), Mistral 7B, DeepSeek-Coder, Claude Sonnet (API).

### 6b-ii. Expose Forge as an MCP server (`--mcp` flag)
- JSON-RPC over stdin/stdout, `--mcp` flag
- Makes Forge usable as a backend from any MCP-compatible IDE (Cursor, Zed, etc.)
- Reuses all existing tool infrastructure

### 6b-iii. VSCode extension (trivial packaging, large surface area)
- `package.json` + launch Forge subprocess + pipe events to webview
- Reuses all existing TUI event infrastructure
- Gives access to VSCode's file tree, git integration, diff viewer

### ✅ 6d. Smarter file selection — COMPLETE

`src/index.rs` — project symbol index, built on every `/plan` invocation (zero model calls):
- Walks project files (Rust, TS/JS, Python, Go, C/C++), extracts top-level symbols: `fn`, `struct`, `enum`, `trait`, `impl`, `class`, `def`, `func`, `const`
- Caps at 500 files, < 100ms, pure regex/text scan
- Injected into plan prompt as a compact file map — model sees real symbol names and paths, not a directory listing
- Post-parse resolution: `files: ["validate_token"]` → scaffold resolves to `src/middleware/jwt.rs` via index
- Model names what it needs; scaffold resolves where it lives
- 7 unit tests: Rust/TS/Python extraction, symbol resolve, ident parsing

### 6e. Mechanical mode (`--mechanical`)
- Pure grep/sed for pattern tasks, zero model calls
- `forge --mechanical "replace foo with bar in src/"` — explicit flag only, never auto-routed
- For rename/replace tasks this is 100x faster and cheaper than any model approach

### ✅ 6f. Telemetry & analytics — COMPLETE
- `src/telemetry.rs` — `SessionStats` (live) + `TaskRecord` (persisted)
- Per-task: input/output tokens, tool calls, compression ratio, model, profile
- Flushed to `.forge/telemetry.jsonl` after every completed agent run (JSONL, appendable, aggregatable)
- **Always-visible stats bar** in TUI — second line below status bar, no toggle needed:
  - `∑ N tasks  X.Xktok  avg Y/task  Z tool calls  W% compressed  peak P%`
  - Dimmed/purple palette so it doesn't compete with active status bar
  - Budget enforcement count and peak context % tracked separately
- Foundation for a hosted dashboard / benchmarking comparisons

---

## File Structure (current)

```
src/
├── main.rs           # clap CLI, single-shot + TUI dispatch
├── client.rs         # HTTP client, SSE streaming, tool call parsing
├── agent.rs          # agent loop, project map, conventions loading, build check
├── history.rs        # tool output compression (model vs display summaries)
├── cache.rs          # file read cache + re-read prevention
├── budget.rs         # proactive token budget, loop detection
├── sessions.rs       # session persistence, JSONL, context injection (8k cap)
├── ui.rs             # tool glyphs
├── config.rs         # profile system, config file load/write
├── mcp.rs            # MCP client — spawn servers, JSON-RPC, tool discovery + dispatch
├── index.rs          # project symbol index — fn/struct/class/impl → file path, used by plan gen
├── telemetry.rs      # SessionStats, TaskRecord, JSONL persistence
├── plan.rs           # plan data structure, step execution, step summaries
└── tools/
    ├── mod.rs         # tool registry + dispatch
    ├── read.rs        # read_file with smart excerpting + symbols=true index
    ├── write.rs       # write_file (overwrite guard)
    ├── edit.rs        # edit_file (fuzzy matching, ±15 line failure hint)
    ├── bash.rs        # bash execution (async, timeout, 200-line cap)
    ├── recall.rs      # retrieve full stored output by id or tool name
    ├── search.rs      # ripgrep wrapper (zero-match → declare done)
    └── list.rs        # list_files
tui/
├── mod.rs            # TUI state, event loop, session browser, plan review, slash commands
└── render.rs         # ratatui draw
```
