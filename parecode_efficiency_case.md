# Why PareCode Uses Fewer Tokens Than the Competition

**The short version:** Most AI coding tools treat every task like it's the first time they've ever seen your project. They read the same files over and over, carry the whole conversation forever, and send you the bill. PareCode doesn't. This document explains exactly how — and puts rough numbers on it.

---

## The Competition's Problem

Tools like OpenCode, Cursor, and Copilot Workspace all operate on the same basic model:

1. User types a task
2. Model explores the codebase by reading files
3. Model makes edits
4. Next task: repeat from step 1 — no memory of what it just learned

Every single task starts blind. The model reads `src/main.rs` again. It reads the types file again. It figures out your project structure again. You pay for all of this, every time.

On a typical 5-file task with a medium-sized project:

| Phase | What happens | Token cost |
|-------|-------------|-----------|
| Orientation | Model reads 3-5 files to understand structure | ~6,000 tokens |
| Work | Actual edits | ~4,000 tokens |
| Verification | Re-reads to check | ~2,000 tokens |
| **Total** | | **~12,000 tokens** |

And on your **20th task** on the same project? Same 12,000 tokens. The model learned nothing.

---

## How PareCode Is Different: 7 Mechanisms

### 1. The Project Graph — "Stop Re-Discovering the Map"

**The problem it solves:** The model's first question on any task is always "what exists and where?" It answers this by reading files. That's expensive.

**What we do:** On first launch, PareCode builds a complete structural map of your project — every file, every function, every struct, every class — and saves it to `.parecode/project.graph`. On every run after that, it loads in milliseconds and checks only what changed (using git's hash database — one subprocess call, not N).

```
project.graph contains:
- 40 source files
- 597 symbols (functions, structs, enums, etc.)
- Which files belong to which cluster (tui, src, tools, etc.)
- Line counts for every file
```

**Token saving:** The model gets this map *before* it reads a single file. Instead of "I need to figure out where authentication lives" → 3 file reads, it's "I can see `fn login()` is in `src/auth/login.rs` at line 4." That's the difference between a tourist and a local.

**Estimated saving per task: ~3,000–6,000 tokens** (2-4 orientation reads eliminated)

---

### 2. The Project Narrative — "Understand Before You Read"

**The problem it solves:** Even with a file map, the model doesn't know *what* each cluster *does*. It knows `tui/` exists but not that it contains the chat interface, the history display, and all the overlay panels.

**What we do:** Once (on first startup), PareCode makes a single model call to generate a plain-English description of the whole project:

```
architecture_summary: "A terminal-based AI coding assistant that provides a TUI
for interacting with LLM agents. Features include file editing, patching, and
reading tools executed by the agent, fuzzy search, git integration..."

cluster_summaries:
  tui: "Terminal UI framework: chat interface, input boxes, spinner animations..."
  tools: "Agent tool implementations: patch, read, and edit operations..."
  src: "Core runtime: API client, git integration, project mapping..."
```

This gets injected into the planning prompt. The model already knows what `src/plan.rs` is *for* before it even considers reading it. It can go straight to the right files.

**Cost:** One model call, ever. Saved to `.parecode/narrative.json`. Warm loads are instant.

**Estimated saving per planning task: ~2,000–4,000 tokens** (cluster exploration eliminated)

---

### 3. Phase-Adaptive Tools — "Only Send What's Needed"

**The problem it solves:** Tool definitions are part of the prompt. Every tool sent to the model costs tokens — even if it never gets used. OpenCode sends all tools every turn.

**What we do (in `src/tools/mod.rs`):**

```
Turn 0-1 (exploration):  read_file, edit_file, bash, search, ask_user, list_files, write_file
Turn 2+ (mutation):      read_file, edit_file, bash, search, ask_user, patch_file
Turn 3+ (with history):  + recall
```

`write_file`, `list_files` are dropped after the exploration phase. `patch_file` (multi-hunk diffs) only arrives once the model has seen the files. `recall` only appears once there's something worth recalling.

There are 9 tools total. The full set costs ~940 tokens per turn. The adaptive set costs ~540 tokens in later turns.

**Estimated saving: ~400 tokens × every turn after turn 2**

For a 10-turn session: ~3,200 tokens saved just from tool definitions.

---

### 4. Smart File Reading — "Don't Read 1,200 Lines When You Need 80"

**The problem it solves:** `read_file("src/tui/mod.rs")` on a 1,200-line file sends 1,200 lines to the model. Most of them are irrelevant.

**What we do (in `src/tools/read.rs`):**

Files under 300 lines are sent in full. Files over 300 lines get a *structured excerpt* instead:

```
[First 40 lines]  — imports, module declarations, type definitions
[Symbol index]    — every function/struct name with its line number (free navigation map)
[Last 60 lines]   — usually contains the most recent additions + test module
```

Total: ~140 lines instead of 1,200. The model can then call `read_file` with `line_range=[245, 280]` to fetch just the function it needs.

Additionally, **line anchors** (4-char hashes like `[a3f2]`) let the model edit a specific line without re-reading the whole file first. No re-read = no tokens.

**Estimated saving per large file read: ~3,500 tokens** (1,060 lines × ~3.3 tokens/line)

For a task touching 3 large files: ~10,500 tokens saved.

---

### 5. Deterministic Budget Enforcement — "Compress, Don't Pay Twice"

**The problem it solves:** Long sessions accumulate tool results in the conversation history. A tool result that returned 800 lines of file content gets sent again on every subsequent turn as part of the conversation. This compounds fast.

What OpenCode does: wait until you hit 90% context capacity, then make *another model call* to summarise the conversation. You pay model prices to save money on model prices. Net saving is often close to zero.

**What we do (in `src/budget.rs`):** Before every API call, PareCode checks if the conversation is over 80% of the token budget. If it is:

**Pass 1 — Compress old tool results:**
```
Old tool result (800 lines, ~2,600 tokens):
"[content compressed — ✓ Read src/agent.rs (800 lines). Ask to recall if needed.]"
(1 line, ~15 tokens)
```

**Pass 2 — Trim oldest conversation turns** (keeping the original task and last 4 messages always).

**No model call. Zero cost.** The compression is deterministic text manipulation.

If the model needs that content back, it calls `recall` — which retrieves the original result from an in-process cache, not from disk.

**Estimated saving:** On a 15-turn session hitting compression at turn 10:
- Without compression: turns 11-15 each carry ~15,000 tokens of stale tool results
- With compression: ~500 tokens of summaries instead
- Saving: ~72,500 tokens across the last 5 turns

---

### 6. Loop Detection — "Stop the Doom Spiral Immediately"

**The problem it solves:** Models occasionally get stuck — they read the same file twice, or try the same edit three times. Each loop turn costs the full per-turn token price.

**What we do (in `src/budget.rs` — `LoopDetector`):** Every tool call is fingerprinted (tool name + first 400 chars of args). If the same fingerprint appears twice in the last 5 calls, the agent loop is terminated and the user is notified.

OpenCode triggers at 3 identical calls. PareCode triggers at 2.

**Estimated saving per loop detected: ~8,000–20,000 tokens** (1-3 wasted turns stopped early)

---

### 7. The Plan/Execute Split — "Surgical Steps, Not Whole-File Dumps"

**The problem it solves:** When you ask an AI to "add authentication to this app", it often loads every file it might need, keeps them all in context for the whole task, and produces a sprawling response. Every file stays in context for every step.

**What we do (in `src/plan.rs`):** Tasks are split into isolated steps. Each step gets *only the files it needs* — nothing else. Step 2 doesn't see the files from Step 1 unless they're explicitly listed.

```
Step 1: "Add AuthConfig struct to src/config.rs"
  → Context: [src/config.rs only]  ~800 tokens

Step 2: "Wire AuthConfig into the agent startup in src/main.rs"
  → Context: [src/main.rs only]  ~1,200 tokens
  + "Step 1: Added AuthConfig struct with fields: endpoint, token, timeout"
  (a one-line summary, not the full file)
```

Without the plan/execute split, both files would be in context for both steps. With it, each step is bounded.

**Estimated saving on a 5-step plan:** Each step saves ~3,000 tokens of cross-contamination. Total: ~12,000 tokens.

---

## How It Compounds Over Sessions (The Real Moat)

Phase 3 (coming) adds **Task Memory**: after every completed task, PareCode records what files were touched and a one-sentence summary of what changed. On your next task, the context package includes:

```
## Recent relevant tasks
- [2d ago] Added AuthConfig to config.rs and wired into agent startup (src/config.rs, src/main.rs)
- [5d ago] Fixed TUI splash animation — replaced static sleep with 120ms ticker (tui/mod.rs)
```

This replaces the model's need to re-discover what was recently changed. It already knows the `AuthConfig` struct exists and where it lives.

**The compounding effect:**

| Session | Without PIE | With PIE (projected) |
|---------|------------|---------------------|
| Task 1  | 12,000 tok | 8,000 tok |
| Task 5  | 12,000 tok | 6,000 tok |
| Task 10 | 12,000 tok | 4,500 tok |
| Task 20 | 12,000 tok | 3,000 tok |

The 20th session row is the moat. Every other tool resets to 12,000. PareCode gets cheaper the more you use it.

---

## Combined Token Budget: Typical Task Comparison

| Mechanism | OpenCode cost | PareCode cost | Saving |
|-----------|--------------|--------------|--------|
| Project orientation | 6,000 | 800 (graph injection) | **5,200** |
| Architecture understanding | 3,000 | 400 (narrative) | **2,600** |
| Tool definitions (10 turns) | 9,400 | 6,200 (adaptive) | **3,200** |
| Large file reads (3 files) | 12,000 | 1,500 (excerpts) | **10,500** |
| History compression (long session) | 0 (pays model to summarise) | 0 (deterministic) | **~5,000** |
| Loop waste (1 loop caught) | 12,000 | 0 | **12,000** |
| Plan/execute isolation (5 steps) | 18,000 | 6,000 | **12,000** |
| **Total** | **~60,400** | **~14,900** | **~45,500 (75%)** |

*These are rough estimates on a medium project (~40 files, ~600 symbols). Real numbers depend on model, project size, and task complexity.*

---

## Why This Isn't Just About Cost

Provider-side prompt caching (Anthropic, OpenAI) will reduce the raw cost argument over time. The quality argument doesn't weaken:

- A model that already knows your architecture summary goes to the right file on the **first try**
- A model with task memory knows that `AuthConfig` was added last week and where — it doesn't hallucinate a different field name
- A model working on an isolated plan step can't accidentally corrupt a file it wasn't supposed to touch

**Fewer tokens = faster responses + fewer mistakes + lower cost.** The token savings are the metric, not the goal.
