# PareCode Efficiency Landscape

Where we already win, where the gaps are, and when to close them.

---

## Where We Already Win (shipped, working today)

### 1. Proactive budget enforcement — not reactive compaction

**The industry problem:** Claude Code, Cursor, OpenCode all let the context fill up, then at ~90% capacity trigger an expensive LLM summarisation call to compact. That call costs tokens to save tokens, introduces latency mid-task, and happens *constantly* on long tasks.

**What we do:** `budget.rs` enforces budget deterministically *before* every API call. Compression is zero-model-call text truncation. The threshold triggers at 80% — you never hit the wall. No surprise compaction mid-execution.

**Measurable delta:** Every compaction avoided saves 2-5K tokens and 3-8 seconds of latency. On a 10-step plan, Claude Code might compact 3-4 times. PareCode: zero.

---

### 2. Tool output compression with recall

**The industry problem:** Every tool result — every file read, every bash output — sits in conversation history verbatim for the entire session. A single `cat large_file.ts` adds ~800 tokens that the model carries forever, even though it only needed it once.

**What we do:** `history.rs` replaces every tool result in history with a one-liner summary immediately after it's processed. Full output stored off-context, retrievable via `recall`. The model carries `read_file src/auth.ts → 89 lines, 3 functions` not the entire file.

**Measurable delta:** A 20-tool-call session that reads 10 files: ~8K tokens of file content compressed to ~200 tokens in history. On every subsequent turn.

---

### 3. Plan/execute isolation — fresh context per step

**The industry problem:** Other agents run the entire task as one long conversation. By step 5, the model's context contains all prior steps' code, all prior reasoning, all tool outputs. Token cost compounds per step.

**What we do:** `plan.rs` executes each step as a fresh agent call. The executor only sees: system prompt (rules) + step instruction + step files. Nothing from previous steps except an optional one-line carry-forward summary.

**Measurable delta:** A 5-step plan where each step reads 2 files (~1K tokens each). Other agents: step 5 context = ~12K tokens accumulated. PareCode step 5 context: ~2.5K tokens. At scale this is a 4-6x difference per step on long plans.

---

### 4. File read caching within a session

**What we do:** `cache.rs` deduplicates file reads within a session. If the model reads `src/auth.rs` twice, the second read returns from cache — no disk I/O, same content.

**Gap (not yet closed):** The cache prevents duplicate disk reads but the *content* still gets sent to the model twice as separate tool results — both summarised, but still two entries. See Quick Win #1 below.

---

### 5. Symbol index for planning

**What we do:** `index.rs` builds a compact project map (file → symbols) injected into the planning prompt. The planner works from a structural overview rather than reading files speculatively.

**Gap (not yet closed):** File sizes and line counts are not currently included. Without them, the model requests files defensively ("I'll read all 5, figure out which 2 I need"). See Quick Win #2 below.

---

### 6. Loop detection

**What we do:** `budget.rs` `LoopDetector` catches when the model makes identical tool calls back-to-back, injects the cached result, and breaks the loop. Other agents spin and burn tokens.

---

### 7. Hash-anchored edits

**What we do:** `edit_file` uses 4-char line hashes as anchors, not line numbers. Edits don't go stale when earlier edits shift line numbers. Other agents get stale-edit errors, re-read the file, retry — 2-3 extra tool calls per stale edit.

---

## Quick Wins — High Impact, Low Effort (implement before PIE)

These don't require PIE. They close obvious leaks in the current architecture.

### QW1: Strip assistant reasoning from history

**The problem:** The model thinks out loud. Every assistant turn contains reasoning like "I need to check auth.rs to understand the token flow, then look at the interceptor..." — CoT is essential for quality but the reasoning about step 3 is irrelevant once step 3 is done.

**Current state:** Assistant reasoning turns stay in history verbatim.

**Fix:** After each tool-call round-trip completes, replace the preceding assistant reasoning turn with a stub: `[reasoning truncated]` or just strip it entirely. Keep only: tool calls, tool results (already summarised), and final assistant responses.

**Where:** `history.rs` or `agent.rs` message processing loop.

**Estimated gain:** 30-50% reduction in history size for typical multi-step tasks. Directly delays or eliminates compaction triggers.

---

### QW2: File sizes in symbol index

**The problem:** `to_prompt_section()` in `index.rs` shows `src/auth.rs: fn validate_token, struct AuthError`. The model doesn't know if that file is 30 lines or 3,000 lines, so it reads defensively.

**Fix:** Add line count to each file entry in the symbol index output:
```
src/auth.rs (89 lines): fn validate_token, struct AuthError
src/handlers/payment.rs (412 lines): fn process_payment, fn refund, fn webhook_handler
```

The model immediately knows not to read the 412-line file for a one-function fix.

**Where:** `index.rs` `to_prompt_section()` — store line count during `build()`, render it in output.

**Estimated gain:** Reduces speculative file reads in planning by ~30-40%. Each avoided read saves 200-800 tokens.

---

### QW3: Executor gets a minimal system prompt

**The problem:** `agent.rs` builds one system prompt (base rules + project map + conventions + git status) and uses it for both planner and executor calls. Every executor step — even a targeted one-function edit — carries the full project map.

**Fix:** Two system prompt variants:
- **Planner:** full system prompt (current behaviour)
- **Executor:** base rules only (~200 tokens). No project map, no symbol index. The executor already has its step files — it doesn't need the rest of the project.

**Where:** `plan.rs` executor call vs `agent.rs` standard call.

**Estimated gain:** ~300-600 tokens per executor step. On a 5-step plan: 1,500-3,000 tokens saved across execution.

---

### QW4: Re-read prevention after failed verification

**The problem:** When a verification fails (build error, test failure), the model almost always re-reads the file it just edited — even though the edit result already contains the current file state. It's reading a file it knows the contents of.

**Fix:** After a failed verification, inject the error output directly alongside a reference to the last edit result rather than as a new prompt turn. Include a note: `"The file state after your last edit is already in context at [tool_call_id]. Fix the error without re-reading."` The system prompt already discourages unnecessary re-reads — this makes it explicit at the failure point.

**Where:** `plan.rs` step verification failure handling.

**Estimated gain:** Eliminates one file read per failed verification attempt (200-800 tokens each). On a plan with 2-3 verification failures, meaningful.

---

### QW5: `/plan resume` as first-class flow

**The problem:** Plans are already persisted to `.parecode/plans/`. But if a plan fails mid-execution and the user retries, they pay full planning tokens again — even for steps already completed.

**Fix:** Surface plan resume more prominently. When `/plan "..."` is typed, check for an incomplete plan for the same project first. Offer to resume from the last failed step. The plan already has `current` index and `StepStatus` — the infrastructure is there.

**Where:** `plan.rs` + TUI command handling.

**Estimated gain:** Full planning cost avoided on retries. On a 5-step plan where step 3 fails, ~1-2K planning tokens saved on retry.

---

### QW6: Mechanical scaffold operations (no model call)

**The problem:** Tasks like "rename `authenticate` to `verify_auth` across the project" get routed through the model. The model reads every file containing the symbol, sends them all as context, gets back edits. 15 files = 15 reads + 15 edits + 15 verifications.

**Fix:** A `scaffold_rename(old_name, new_name)` operation that uses the symbol index to find all occurrences and does pure text substitution. Zero model calls for the rename itself. Model is only invoked if the operation encounters ambiguity.

Pattern generalises: any task that's a pure deterministic text transformation (rename, add import, remove dead code matching a pattern) should be a scaffold operation, not a model task.

**Where:** New `src/scaffold.rs`, wired into task classification in `agent.rs`.

**Estimated gain:** Eliminates entire task classes from the token economy. A project-wide rename goes from 5-15K tokens to ~0.

---

## Where PIE Fits

PIE is not a replacement for the above — it's the next layer after the leaks are closed.

```
Today (already shipped)          Quick Wins (close the leaks)      PIE (compound knowledge)
─────────────────────────        ──────────────────────────────    ──────────────────────────
Proactive budget enforcement     Strip reasoning from history       Persistent symbol graph
Tool output compression          File sizes in symbol index         Cluster narrative
Plan/execute isolation           Minimal executor system prompt     Task memory
File read cache                  Re-read prevention                 Context weight learning
Symbol index for planning        /plan resume                       Keyword grep anchoring
Loop detection                   Mechanical scaffold ops            Vague task anchoring
Hash-anchored edits
```

**The quick wins make each task cheaper. PIE makes each session cheaper than the last.**

They stack. Closing the leaks first means PIE's baseline is already 30-40% better than it would be without them — and the learning loop compounds from a cleaner starting point.

---

## Implementation Order

| # | Work | Type | Effort | When |
|---|------|------|--------|------|
| 1 | Strip assistant reasoning from history | Quick Win | 1 day | Now |
| 2 | File sizes in symbol index | Quick Win | 2 hours | Now |
| 3 | Minimal executor system prompt | Quick Win | half day | Now |
| 4 | Re-read prevention after failed verify | Quick Win | half day | Now |
| 5 | `/plan resume` surfacing | Quick Win | 1 day | Soon |
| 6 | Mechanical scaffold ops (rename/replace) | Quick Win | 2-3 days | Soon |
| 7 | PIE Phase 1 — Persistent graph | PIE | 1 week | After QW1-4 |
| 8 | PIE Phase 2 — Cluster narrative | PIE | 1 week | After Phase 1 |
| 9 | PIE Phase 3 — Task memory + learning | PIE | 1 week | After Phase 2 |
| 10 | PIE Phase 4 — Polish + observability | PIE | 3 days | After Phase 3 |

**Do QW1-4 first.** They're small, independent, and immediately measurable. They also make the token economics story in the README actually demonstrable before PIE ships — you can benchmark against Claude Code today.

QW5-6 are slightly larger but also pre-PIE. Resume makes the current plan system production-grade. Scaffold ops prove the "scaffold handles mechanical tasks, model handles judgment" principle before PIE embeds it deeply.

---

## The Headline Numbers (conservative, measurable)

| Scenario | Claude Code / Cursor | PareCode today | PareCode + QWs | PareCode + QWs + PIE |
|----------|---------------------|----------------|-----------------|----------------------|
| Single task, 5 files | ~40K tokens | ~8K tokens | ~5K tokens | ~3K tokens |
| 5-step plan, medium project | ~80K tokens | ~20K tokens | ~12K tokens | ~6K tokens |
| Same task, 20th session | ~80K tokens | ~20K tokens | ~12K tokens | ~3K tokens |
| Large codebase (100+ files) | ~100K+ tokens | ~25K tokens | ~15K tokens | ~4K tokens |

The "20th session" row is the moat. No other tool gets cheaper over time.
