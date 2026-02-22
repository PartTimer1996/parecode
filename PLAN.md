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

## Phase 6b — Distribution & First-Run Experience 

The Rust binary is Forge's biggest distribution advantage. Every competitor requires a language runtime: OpenCode and Claude Code need Node.js, Aider needs Python, oh-my-opencode needs both. Forge is a single static binary — zero dependencies, starts in <10ms. The goal: install to productive in under 60 seconds, better than any competitor.

### 6b-i. Binary releases with cargo-dist FOURTH NEXT! - TEST MYSELF - install setup, qwen scenarios, then Claude

**cargo-dist** automates the entire release pipeline from a single `dist init`. On every version tag push, GitHub Actions builds all targets, produces platform installers, updates the Homebrew tap, and creates the GitHub Release — zero manual steps.

**Target matrix:**
| Target | Platform | Notes |
|---|---|---|
| `x86_64-unknown-linux-musl` | Linux x86_64 | Statically linked — works on any Linux, any glibc version |
| `aarch64-unknown-linux-musl` | Linux ARM64 | AWS Graviton, Raspberry Pi, ARM servers |
| `x86_64-apple-darwin` | macOS Intel | Older Macs |
| `aarch64-apple-darwin` | macOS Apple Silicon | M1/M2/M3 — now majority of Macs |
| `x86_64-pc-windows-msvc` | Windows x86_64 | Primary Windows target |

**musl is non-negotiable for Linux.** Statically linked = no "error while loading shared libraries" ever. This eliminates the most common class of post-install failures on Linux.

**Cargo.toml / dist.toml configuration:**
```toml
[workspace.metadata.dist]
cargo-dist-version = "0.30.4"
ci = ["github"]
installers = ["shell", "powershell", "homebrew"]
tap = "PartTimer1996/homebrew-forge"
targets = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
]
publish-jobs = ["homebrew"]

[profile.dist]
inherits = "release"
lto = "thin"
```

**Release process:** `git tag v0.1.0 && git push --tags` — that's it.

**What cargo-dist produces automatically:**
- GitHub Release with 5 platform binaries + SHA256 checksums for each
- Shell installer script (`forge-installer.sh`) with checksum validation
- PowerShell installer script (`forge-installer.ps1`) for Windows
- Homebrew formula pushed to `PartTimer1996/homebrew-forge` tap

### 6b-ii. Install methods (README-ready)

```bash
# macOS / Linux — one-liner, zero dependencies
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/PartTimer1996/Forge/releases/latest/download/forge-installer.sh | sh

# macOS — Homebrew
brew install PartTimer1996/forge/forge

# Windows — PowerShell
irm https://github.com/PartTimer1996/Forge/releases/latest/download/forge-installer.ps1 | iex
```

**Competitive install comparison:**
| Tool | Install command | Requires |
|---|---|---|
| **Forge** | `curl ... \| sh` | Nothing |
| OpenCode | `npm install -g opencode` | Node.js |
| oh-my-opencode | npm + manual agent config | Node.js + setup time |
| Claude Code | `npm install -g @anthropic-ai/claude-code` | Node.js |
| Aider | `pip install aider-chat` | Python |
| Plandex | `curl ... \| bash` | Nothing (also compiled binary) |

Forge and Plandex are the only zero-dependency installs in the category.

### 6b-iii. Distribution channel rollout

**Week 1 (ship with first release):**
- GitHub Releases (cargo-dist, automated)
- Shell installer (cargo-dist, automated)
- Homebrew tap (cargo-dist, automated)

**Week 2:**
- **AUR** (`forge-bin`) — binary PKGBUILD, targets Arch Linux developers. Highly technical early-adopter audience. Minimal maintenance: update `pkgver` + `sha256sums` on each release.
- **WinGet** — pre-installed on Windows 11. `wingetcreate new <release-url>` generates the manifest; `vedantmgoyal9/winget-releaser` GitHub Action automates future updates.
- **Shell completions** — generate for bash/zsh/fish via clap's `generate` feature. Included in the tarball, install instructions in README. Makes Forge feel native.

**Later (when users ask):**
- `flake.nix` for Nix users — provide in repo, they can `nix profile install github:PartTimer1996/Forge`
- nixpkgs submission — often happens organically when the tool gains traction
- deb/rpm — only worth building if significant Ubuntu/Fedora user base requests it

**Do not bother:**
- Snap (sandboxing breaks tool, wrong audience)
- Flatpak (designed for GUI apps)
- Docker (not a server application)
- npm/pip wrappers (adds maintenance surface for marginal gain)

### 6b-iv. `forge update` self-upgrade command

curl-installed users have no package manager to update through. `forge update` re-runs the install script against latest, replaces the binary in-place.

```
$ forge update
Checking for updates... forge 0.1.0 → 0.2.1 available
Downloading forge 0.2.1 for aarch64-apple-darwin... ✓
Verifying checksum... ✓
Replacing /home/user/.local/bin/forge... ✓
forge 0.2.1 installed.
```

Implementation: `src/main.rs` — `--update` subcommand, fetches GitHub API `/releases/latest`, compares version, re-runs platform-specific installer script.

### 6b-v. Benchmarking suite

Run on the tasks that caused Qwen3 14B to loop in OpenCode. Record token counts, tool calls, success rate, wall time. Publish results — this is the "viral moment" that proves the token efficiency claim.

| Task | Target |
|---|---|
| `"remove all console.log from src/"` | ≤ 5 tool calls, < 5k tokens |
| `"rename columns → allColumns in data-table.component.ts"` | No re-reads, clean 1-shot |
| `"reorganise SCSS in header.component.scss"` | < 3k tokens |

Model matrix: Qwen3 14B (Ollama), Mistral 7B, DeepSeek-Coder, Claude Sonnet (API). Publish side-by-side with OpenCode numbers.

### 6b-vi. Expose Forge as an MCP server (`--mcp` flag)
- JSON-RPC over stdin/stdout, `--mcp` flag
- Makes Forge usable as a backend from any MCP-compatible IDE (Cursor, Zed, etc.)
- Reuses all existing tool infrastructure

### 6b-vii. VSCode extension (trivial packaging, large surface area)
- `package.json` + launch Forge subprocess + pipe events to webview
- Reuses all existing TUI event infrastructure
- Gives access to VSCode's file tree, git integration, diff viewer

---

## Phase 6c THIRD NEXT! — First-Run Experience (install → productive in 60 seconds)

**The target flow:**
```
install → forge → interactive setup → working
```

**Nobody's current flow:**
```
install → run → error: no config → read docs → create config → run again → maybe works
```

Forge should be the tool that just works.

### 6c-i. First-run detection and setup wizard

When `forge` is launched with no config file present, run an interactive setup wizard instead of erroring:

```
Welcome to Forge ⚒

No config found at ~/.config/forge/config.toml. Let's get you set up.

? How do you want to run Forge?
  ❯ Local (Ollama) — free, private, works offline
    Anthropic Claude — best quality, requires API key
    OpenAI — GPT-4o, requires API key
    OpenRouter — any model, one API key
    Skip — I'll configure manually

[If Ollama selected — after silently probing localhost:11434]
  Checking for Ollama... ✓ found (3 models installed)

? Which model?
  ❯ qwen3:14b   (recommended for coding tasks)
    qwen2.5-coder:14b
    llama3.1:8b

Config written to ~/.config/forge/config.toml ✓
Running /init to detect project context... ✓ written to .forge/conventions.md

Ready. What would you like to build?
▶
```

**Auto-detection shortcuts (skip the wizard entirely):**
- If `ANTHROPIC_API_KEY` env var present → auto-configure Claude profile, skip wizard
- If `OPENAI_API_KEY` env var present → auto-configure OpenAI profile, skip wizard
- If Ollama responds at `localhost:11434` with models → default to local, only ask which model
- If only one model installed → skip even that question, just use it

**Implementation:**
- `src/setup.rs` — `run_setup_wizard() -> ResolvedConfig` — terminal prompts (no TUI, runs before TUI starts)
- `src/main.rs` — check `config_path().exists()` before launching TUI; if missing, run wizard first
- Wizard uses `dialoguer` crate for interactive prompts (or hand-rolled crossterm prompts to avoid extra dependency)

### 6c-ii. Ollama auto-detection

On every startup (not just first run), silently probe `localhost:11434/api/tags` (100ms timeout). If Ollama is running:
- Show `◉ Ollama` indicator in TUI status bar when using local profile
- If user is on a cloud profile but Ollama is also running: show soft hint `◉ Local models available — /profile local to switch`
- On first run: Ollama presence triggers local-first default in the wizard

### 6c-iii. `/init` auto-prompt on new project

On first `forge` launch in a directory with no `.forge/` folder:

```
No project conventions found.
Run /init to prime Forge with your project's stack and style? [Y/n]
```

If Y: runs `/init` inline (see Phase 6i), shows result, asks to save. If N: continues normally, can run `/init` later.

### 6c-iv. `forge update` and version awareness

Status bar shows version and available update indicator:
```
forge 0.1.0 · new version 0.2.1 available — run `forge update`
```

Checked once per session against GitHub API (cached for 24h in `~/.local/share/forge/update-check`). Never blocks startup.

### 6c-v. Shell completion install hint

On first run after install, if completions aren't installed:
```
Tip: install shell completions for tab-completion of commands and flags:
  forge --completions zsh > ~/.zfunc/_forge   # zsh
  forge --completions bash > ~/.bash_completion.d/forge  # bash
  forge --completions fish > ~/.config/fish/completions/forge.fish  # fish
```

Shown once, suppressed after. Completions generated via clap's `generate` feature, shipped in release tarballs.

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

## ✅ Phase 6g — Hash-Anchored Edits (correctness) — COMPLETE

**The single biggest correctness improvement available.** Inspired by oh-my-opencode's hash-anchored edit validation, which moved task success from 6.7% → 68.3% on complex tasks. Stale-line edits — where the file has shifted since it was read — are the most common silent failure mode.

**How it works:**a
- `read_file` output annotates each line with a short content hash: `42#a3f: fn validate_token(...)`
- Hashes are compact (4–5 chars), placed at the start of the line number field — subtle, not noisy
- `edit_file` accepts an optional `anchor` hash alongside `old_str`
- Before applying: verify the hash still matches the line at the expected position
- If hash mismatch → return error: `"Anchor mismatch at line 42 — file has changed since last read. Re-read to get current hashes."`
- If no anchor provided → fall through to existing fuzzy matching (backwards compatible)

**Implementation:**
- `src/tools/read.rs` — hash generation (CRC32 or FNV-1a of the line content, base36, 4 chars)
- `src/tools/edit.rs` — anchor verification before fuzzy match
- `src/cache.rs` — cache stores hashes alongside content; invalidated on write/edit
- Hash format: `{line_num}#{hash}:` prefix — stripped before content is used

**Design constraints:**
- Hashes must be invisible to the model's reasoning (it should use them for anchoring, not describe them)
- System prompt addition: `"Each line in read_file output is prefixed {line}#{hash}: — use the hash as an anchor in edit_file calls to prevent stale-line errors"`
- Backwards compatible: anchor param is optional; existing edit calls continue to work

---

## Phase 6h — SECOND NEXT! Hooks System

**First-class workflow automation.** Config-driven pre/post hooks that run deterministic shell commands at key points in the agent lifecycle. The UX priority: hooks should feel instant and integrated, not like a CI config bolted on.

**Hook events:**
| Event | Trigger | Common use |
|---|---|---|
| `on_task_done` | After every completed agent run | `cargo test`, `npm test` |
| `on_edit` | After any `write_file` or `edit_file` call | `cargo check`, `tsc --noEmit` |
| `on_plan_step_done` | After each plan step completes | lint, format |
| `on_session_start` | TUI startup | `git pull`, environment check |
| `on_session_end` | TUI quit or `/new` | `git status` summary |

**Config (per-profile or global):**
```toml
[hooks]
on_edit      = ["cargo check -q"]
on_task_done = ["cargo test --quiet 2>&1 | tail -5"]
on_session_end = ["git status --short"]
```

**UX behaviour:**
- Hook output shown inline in conversation history as a dimmed `⚙ hook:` block — not mixed with agent output
- If hook exits non-zero: output shown in amber, model NOT automatically reinvoked (user decides)
- Hooks run with a 30s timeout; timeout shown as warning not error
- `/hooks` slash command lists configured hooks and their last exit codes
- Hooks can be disabled per-session: `/hooks off`

**Implementation:**
- `src/hooks.rs` — `HookRunner { config: HookConfig }`, `run(event: HookEvent) -> HookResult`
- `src/config.rs` — `HookConfig` struct added to `Profile`
- `src/tui/mod.rs` — fire hooks at `AgentDone`, post tool dispatch, session start/end
- Hook output sent as `AppEvent::HookOutput { event, stdout, exit_code }` to render layer

---

## ✅ Phase 6i — `/init` Command — COMPLETE

**One-shot project context priming.** Walks the project and auto-generates `.forge/conventions.md` from existing project files. Eliminates manual conventions setup for new projects.

**Sources (in priority order):**
1. `README.md` — first 50 lines (project description, stack, install)
2. `Cargo.toml` / `package.json` / `pyproject.toml` / `go.mod` — name, language, key dependencies
3. `AGENTS.md` / `CLAUDE.md` — if already exists, merge rather than overwrite
4. `.eslintrc` / `rustfmt.toml` / `pyproject.toml [tool.ruff]` — style rules detected
5. Test directory structure — infer test runner from `jest.config`, `pytest.ini`, `#[cfg(test)]`

**Output format (`.forge/conventions.md`):**
```markdown
# Project: my-app
Language: TypeScript (Bun runtime)
Test runner: `bun test` — tests in `src/__tests__/`
Lint: `eslint src/` — run after edits
Key dependencies: React 19, Drizzle ORM, Hono

## Style
- Prefer `const` over `let`
- No default exports
- Zod for all external input validation
```

**TUI integration:**
- `/init` slash command — runs inline, shows progress, opens result in pager overlay for review/edit before saving
- On first `forge` run in a new directory (no `.forge/` present): prompt "No conventions found. Run `/init` to prime project context? [y/N]"
- `forge --init` CLI flag (already exists for config) — extend to also run project init if in a project directory

**Implementation:**
- `src/init.rs` — `run_project_init(cwd) -> String` — pure text extraction, no model calls
- `src/tui/mod.rs` — `/init` command handler, first-run prompt

---

## ✅ Phase 6j — Cost Estimation in Plan Overlay — COMPLETE

**Pre-task cost transparency.** Before running a plan, show an estimated token cost and (optionally) API cost. Nobody does this. Users burned $638+ in 6 weeks on AI agents without forewarning.

**Estimation method (no model call, heuristic):**
- Per step: `base_tokens (500) + sum(file_sizes_in_step / 4) + instruction_len / 4`
- Total: `sum(step_estimates) × 1.3` (overhead factor for tool results and responses)
- API cost: `total_tokens × rate_per_token` — rates configured per-profile, or use known defaults (Haiku: $0.25/Mtok input)

**Plan overlay addition:**
```
┌─ Plan: add JWT authentication ────────────────────────┐
│ 4 steps  ·  est. 12k–18k tokens  ·  ~$0.004 at Haiku │
│                                                        │
│ ▶ Step 1: Add JWT dependency to Cargo.toml            │
│   Step 2: Implement token validation middleware        │
│   ...                                                  │
```

**Config:**
```toml
[profiles.claude]
cost_per_mtok_input  = 0.25   # optional, enables cost display
cost_per_mtok_output = 1.25
```

**Implementation:**
- `src/plan.rs` — `estimate_plan_cost(plan, index) -> CostEstimate { tokens_low, tokens_high, usd }`
- `src/tui/render.rs` — add estimate row to plan overlay header
- `src/config.rs` — `cost_per_mtok_input/output` optional fields on `Profile`

---

## ✅ Phase 6k — Quick Mode / Tiered Autonomy — COMPLETE

**Right-sized agent for right-sized tasks.** The full agent loop (plan → load context → multi-turn tool loop → verify) is overkill for a one-line fix. Quick mode skips the overhead entirely.

**Trigger:**
- `forge --quick "task"` — explicit flag
- Auto-detect heuristic (opt-in via config `auto_quick = true`): task < 20 words, no file `@` attachments, no `/plan` prefix → quick mode
- `/quick "task"` in TUI

**Quick mode behaviour:**
- Single API call — no multi-turn loop
- No plan generation, no step isolation
- Context: system prompt + task only (no file loading, no session history)
- Tools available: `edit_file`, `bash` (read-only commands only), `search`
- Max 1 tool call before returning to user
- Token target: < 2k tokens total
- TUI: shows `⚡ quick` badge in status bar instead of spinner

**When NOT to use quick mode:**
- Task contains words like "refactor", "add feature", "implement", "plan" → warn and suggest normal mode
- Task references multiple files → warn

**Implementation:**
- `src/agent.rs` — `run_quick(task, config) -> AgentResult` — simplified single-shot path
- `src/main.rs` — `--quick` flag, auto-detect logic
- `src/tui/mod.rs` — `/quick` command, badge in status bar

---

## Phase 7 — Advanced Orchestration

### 7a. Automatic model routing by category

Extend `planner_model` into a full `model_routes` table. Tasks and plan steps declare a category; the harness picks the right model automatically.

**Categories:**
| Category | Profile model example | When used |
|---|---|---|
| `deep` | `claude-opus-4-6` | Complex multi-file refactors, architecture decisions |
| `standard` | `claude-sonnet-4-6` | Default — most coding tasks |
| `quick` | `claude-haiku-4-5-20251001` | Single-file edits, quick queries |
| `search` | cheapest available | Web search, grep, read-only research |

**Config:**
```toml
[profiles.claude.model_routes]
deep     = "claude-opus-4-6"
standard = "claude-sonnet-4-6"
quick    = "claude-haiku-4-5-20251001"
search   = "claude-haiku-4-5-20251001"
```

**Integration with plan steps:**
- Plan generation adds a `category` field to each step based on instruction complexity
- Agent loop selects model per step rather than once per session
- Quick mode auto-routes to `quick` category

### 7b. Background parallel plan steps

Execute independent plan steps concurrently. Sequential by default; parallel only when steps have no file overlap.

**Dependency analysis (static, no model call):**
- Build a directed graph: step A → step B if B lists a file that A modifies
- Steps with no shared files and no dependency edge → eligible for parallel execution
- Max concurrency: configurable `parallel_steps = 3` in config (default: 1 = sequential)

**Execution:**
- `tokio::spawn` per eligible step group
- Each step gets its own `McpClient` scope (MCP connections not shared across parallel steps)
- Results collected in order; step summaries merged before next sequential step
- TUI shows parallel steps as a grouped block with individual ✓/✗ per step

**Constraints:**
- Steps that call `bash` with side effects are always sequential (conservative)
- File write conflicts → pause, surface to user for resolution
- Requires 7a (model routing) to be useful — parallel steps should use `quick`/`search` routes

### 7c. MCP skill scoping

Scope MCP servers to specific plan step categories or task keywords rather than loading all servers globally.

**Config:**
```toml
[[profiles.local.mcp_servers]]
name    = "playwright"
command = ["npx", "-y", "@playwright/mcp"]
scope   = ["visual", "frontend", "test-e2e"]   # only loaded for these categories
```

**Behaviour:**
- At plan step start: check step category against each server's `scope`
- Only matching servers included in tool list for that step
- Reduces tool list size by 60-80% for non-matching steps — keeps model focused

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
