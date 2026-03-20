# PIE v2 Implementation Guide — Flow Paths & Soft Edges

## For Ryan + Implementing Agent

**Goal:** Build on the existing `symbol_index` and `find_symbol` tool to eliminate model-driven discovery. The model should receive relevant code automatically before its first turn, instead of calling `find_symbol` 3-5 times to follow a thread.

**Starting point:** PareCode already has a working symbol index with fuzzy matching, line numbers, lengths, attributes, and call edges between symbols. The `find_symbol` tool gives the model instant lookups. PIE v1 provides narrative generation, cluster detection, and cached reads.

**What we're building:** Two new capabilities layered on top of the existing index.

---

## The Core Problem

The model currently drives discovery. Even with PIE's fast `find_symbol`, the pattern is:

```
Model: find_symbol("calculateTotal")     → reads result
Model: "this calls DiscountEngine..."
Model: find_symbol("DiscountEngine.apply") → reads result  
Model: "and that uses TaxCalculator..."
Model: find_symbol("TaxCalculator.compute") → reads result
Model: "okay NOW I understand, here's my plan"
```

Three round trips. The model is rediscovering what the symbol index already knows — that these functions call each other. The call edges are right there in the graph. We just don't use them proactively.

---

## What We're Adding

### Feature 1: Flow Paths

**What:** Pre-computed call chains stored alongside the symbol index. Walk forward from entry points following the call edges that already exist in the graph.

**Why:** When a user says "the total is wrong," the scaffold matches to a flow path and pre-loads the entire relevant call chain into the model's first prompt. Zero `find_symbol` calls needed.

### Feature 2: Soft Edges

**What:** Additional relationship data computed from type overlap, name similarity, and git co-change history. Stored as lightweight associations between existing symbols.

**Why:** When there's no direct call edge between two related pieces of code, soft edges help the context assembler include them anyway. Covers cases flow paths miss.

### Feature 3: Prompt-Aware Assembly

**What:** Before injecting context, check what the user already provided. Only add what's genuinely new.

**Why:** If the user already attached a file and described the problem in detail, dumping more context on top is noise. The scaffold fills gaps, it doesn't pad.

---

## Implementation Order

Build these in sequence. Each one is independently useful and testable.

```
Phase 1: Flow Path Tracing        → stored data, no model interaction yet
Phase 2: Flow Path Matching        → match user queries to paths  
Phase 3: Automatic Context Delivery → inject matched path into prompt
Phase 4: Prompt-Aware Assembly      → scale enrichment to user's specificity
Phase 5: Soft Edges                 → extend reach beyond direct call chains
```

---

## Phase 1: Flow Path Tracing

### What to Build

A function that runs after the symbol index is built (or updated). It walks forward from entry points following call edges and stores the resulting chains.

### Entry Points

Entry points are functions that START a flow — they're triggered externally, not called by other internal code. Detection depends on the language but the concept is universal:

- Functions with zero incoming internal call edges (nothing in the project calls them)
- Framework-specific patterns: Angular component methods bound in templates, Express/Actix route handlers, exported `main` or `pub fn` in Rust, React component top-level functions
- Event handlers: onClick, onSubmit, lifecycle hooks

For a first pass, simply finding functions with zero or very few incoming call edges from within the project is good enough. Framework-specific detection can be refined later.

### The Walk Algorithm

From each entry point, walk forward following outgoing call edges in the symbol index:

```
Start at entry point
For each function it calls:
  If it's internal to the project (not a library/external call):
    If it doesn't have an extremely high incoming edge count (skip utilities):
      Add it to the path
      Recurse from this function
  Track visited symbols to prevent infinite loops on cycles
```

**Skip utility functions.** If a function has 15+ incoming call edges, it's probably a logger, validator, or formatter that everything uses. It's not part of any specific flow — it's shared infrastructure. Including it in every path adds noise. The threshold (15+) can be tuned but start there.

**Prevent cycles.** Track visited symbols per path walk. If you hit something already visited, stop that branch.

**Minimum path length.** Only store paths with 2+ steps. A single function with no internal calls isn't a "flow."

### What to Store

Each path is a lightweight structure referencing symbols that already exist in the index:

```
FlowPath:
  id: generated unique string
  name: auto-generated from entry point + terminal symbol names
  entry_point: SymbolId (reference to existing index)
  steps: Vec<SymbolId> (ordered list, references to existing index)
  keywords: Vec<String> (extracted from function names, param names, type names)
```

**Keyword extraction:** Split camelCase and snake_case symbol names into words. `calculateCartTotal` becomes ["calculate", "cart", "total"]. `DiscountEngine` becomes ["discount", "engine"]. These keywords are what we match against user queries later.

**Path naming:** Use the entry point name and the last step name. E.g., "CheckoutComponent.submit → CartAPI.postOrder". Doesn't need to be perfect — it's for display in narrowing questions, not for matching.

### Storage

Store in `.parecode/paths.json` alongside existing index data. The file is small — each path is just a list of symbol ID references plus keywords. A project with 20 paths might be 5-10KB.

### Incremental Updates

When the symbol index updates (files changed, new edges detected), re-trace paths that include any modified symbol. Don't rebuild all paths — just the affected ones. If a function gains a new outgoing call edge, re-trace any path containing that function.

### Deduplication

Two paths that share 80%+ of their steps are essentially the same flow entered from different points. Merge them — keep both entry points but store a single step list. This prevents the path index from bloating with near-duplicates.

### Expected Output

For a medium project, expect 15-30 flow paths after deduplication. Each one represents a meaningful "thing the application does" — checkout, login, profile update, search, etc.

### How to Test Phase 1

Run the tracer on PareCode's own codebase or a familiar project. Inspect the output paths manually. Do they make sense? Do they capture the main features? Are any obviously missing? Are utilities correctly excluded? This is the foundation — get it right before building matching on top.

---

## Phase 2: Flow Path Matching

### What to Build

A function that takes the user's task description (and any file/symbol references they provided) and matches it against the stored flow paths. Runs before the model's first turn.

### Matching Logic

This is the same kind of fuzzy matching `find_symbol` already does, applied to paths instead of individual symbols:

1. Tokenise the user's input into keywords (split on spaces, camelCase, snake_case)
2. For each flow path, score keyword overlap between user keywords and path keywords
3. Boost score if user explicitly referenced a file or symbol that appears in the path
4. Boost score if a resolution log entry matches (see below)

### Three Outcomes

**Clear match (one path scores significantly higher than others):**
Proceed to Phase 3 — assemble context from this path automatically.

**Ambiguous (two or three paths score similarly):**
Ask the user ONE narrowing question. Template-based, no model call:
```
"I see a few areas that could be involved:
  1. Cart total calculation (DiscountEngine → TaxCalculator → PriceRounding)
  2. Item quantity updates (CartStore → CartService)
  3. Not sure — let me explore
Which is closest?"
```
User picks one, proceed to Phase 3. "Not sure" falls back to current PIE behaviour.

**No match (no path scores above threshold):**
Fall back to current PIE behaviour entirely. The model drives discovery with `find_symbol` as it does today. No regression.

### Always Include "Not Sure"

The narrowing question must always have an escape hatch. The scaffold's understanding might be wrong. The user should never feel forced into a path that doesn't match their intent.

### How to Test Phase 2

Feed various task descriptions and verify matching. "The total is wrong" should match a cart/checkout path. "Login is broken" should match an auth path. "The button colour is wrong" probably shouldn't match any path (UI-only, no data flow). Test edge cases where multiple paths share keywords.

---

## Phase 3: Automatic Context Delivery

### What to Build

When a path matches, extract code for the relevant symbols and inject it into the model's initial prompt. The model never calls `find_symbol` for these — the code is already there.

### What the Model Receives

The matched path's symbols get extracted from the index and included in the prompt before the model's first turn:

```
RELEVANT CODE PATH: [path name]
Flow: [entry] → [step1] → [step2] → [step3] → [terminal]

--- [step1 name] ([file]:[start_line]-[end_line]) ---
[extracted code from symbol index]

--- [step2 name] ([file]:[start_line]-[end_line]) ---
[extracted code from symbol index]

--- [step3 name] ([file]:[start_line]-[end_line]) ---
[extracted code from symbol index]
```

### Scoping Rule

Don't dump every step in the path. Apply a simple depth rule based on distance from the user's explicit target:

- **The function the user mentioned (or the entry point if none specified):** Full body
- **One hop out (functions it directly calls):** Signature + return type + full body if small (<30 lines), otherwise just signature
- **Two hops out:** Name and signature only, enough for the model to know it exists and can request more via `find_symbol` if needed

This keeps the injected context focused. A 6-step path doesn't dump 6 full function bodies — it dumps maybe 2-3 bodies and a few signatures.

### Token Budget

Set a budget for auto-injected path context. Suggested starting point: 1500 tokens max. If the path would exceed this, include fewer steps (prioritise by proximity to the user's target). The model can always request more via `find_symbol` — the injected context is a head start, not the entire picture.

### Model's Tools Don't Change

`find_symbol` and any other existing tools remain available. The model can still call them if the pre-loaded context isn't enough. The difference is: previously the model HAD to call them for discovery. Now it usually doesn't need to because the scaffold already provided the relevant chain.

### Resolution Log Entry (Lightweight)

If the resolution log (see below) has a relevant past entry, include it as one line:
```
Previous related fix: null check missing in DiscountEngine.apply:52
```

This is the only "learning" output the model sees directly, and it works because it reads like a colleague's note — plain language, immediately actionable.

### How to Test Phase 3

Run a task on a project with flow paths built. Compare: how many `find_symbol` calls does the model make WITH path context injected vs WITHOUT? Target: 60-70% reduction in discovery tool calls.

---

## Phase 4: Prompt-Aware Assembly

### What to Build

Before injecting path context, check what the user already provided. Scale enrichment inversely to prompt specificity.

### The Coverage Check

Extract from the user's prompt:
- File paths mentioned or attached
- Symbol names referenced
- Line numbers mentioned

Compare against what the matched path would inject. For each piece of path context:

- **User already attached this file:** Don't include the code excerpt from it. The model already has the file. Maybe include a one-line pointer: "Note: [function name] at line [N] is part of this flow" — just to orient the model within the file they provided.
- **User mentioned this symbol by name:** Don't include its code. They know about it. Include connected symbols they DIDN'T mention.
- **User provided no file or symbol references:** Full path context injection as in Phase 3.

### The Principle

The scaffold fills gaps. If the user gave a detailed, specific prompt with files attached and exact function references, the scaffold adds almost nothing — maybe a resolution log note and a couple of connected function signatures. If the user gave a vague "it's broken," the scaffold provides heavy enrichment.

This doesn't require classifying prompts into categories. Just check overlap. High overlap = light touch. Low overlap = heavy enrichment. It falls out naturally.

### How to Test Phase 4

Give the same task with different levels of detail:
1. "it's broken" — should get full path context
2. "DiscountEngine.apply is broken" — should get path context minus DiscountEngine (user knows about it), plus connected functions
3. "DiscountEngine.apply line 52 has a null check bug, here's the file" — should get minimal additions, maybe just resolution log note

Verify the model gets appropriately different context in each case.

---

## Phase 5: Soft Edges

### What to Build

Additional relationship data between symbols that don't have direct call edges. Computed from three signals using data you already have or can cheaply obtain.

### Signal 1: Type Affinity

The symbol index already stores parameter types and return types as strings. Compare them across symbols:

```
UserService.getProfile(id: string) → Observable<User>
UserCacheService.getCached(id: string) → User | null
AdminService.updateUser(user: User) → Observable<void>

Shared type "User" creates soft edges between all three.
```

Implementation: after the symbol index is built, iterate through all callable symbols. For each pair that shares a non-trivial type (skip `string`, `number`, `boolean`, `void` — too common), create a soft edge. Weight by how rare the shared type is — `User` appearing in 5 functions is more meaningful than `string` appearing in 500.

### Signal 2: Name Similarity

You already extract keywords for flow paths by splitting camelCase/snake_case. Reuse this. Symbols whose name tokens overlap are likely in the same domain:

```
calculateCartTotal → ["calculate", "cart", "total"]
recalculateCartAfterDiscount → ["recalculate", "cart", "after", "discount"]
validateCartItems → ["validate", "cart", "items"]

All share "cart" — soft edge between them.
```

Skip very common words that appear in many symbol names. Focus on domain-specific words that appear in only a few symbols.

### Signal 3: Git Co-Change

Run once during init:
```
git log --format="%H" --name-only
```

Parse the output. For each commit, record which files changed together. Build a co-change frequency between file pairs:

```
auth.service.ts + auth.interceptor.ts: 14 co-commits → strong soft edge
auth.service.ts + cart.service.ts: 0 co-commits → no soft edge
avatar.component.ts + image-cache.service.ts: 8 co-commits → medium soft edge
```

Map file-level co-change to symbol-level soft edges (symbols in co-changing files get the soft edge).

This is the most powerful signal because it captures relationships that no static analysis can see — like a config file that always changes alongside the feature it configures, or a test helper that always changes with the module it tests.

### Storage

Soft edges are stored alongside the symbol index. Each is:

```
SoftEdge:
  from: SymbolId
  to: SymbolId  
  affinity: f32 (0.0 to 1.0)
  reasons: Vec<String> (e.g., ["shared_type:User", "git_cochange:8"])
```

Don't store every possible pair. Only store edges above a minimum affinity threshold. This keeps the data manageable.

### How the Context Assembler Uses Soft Edges

**The model never sees soft edges.** They're internal to the scaffold.

When the context assembler is building a context package — either from a matched flow path or from current PIE behaviour — it uses soft edges to extend its reach:

1. Start with the symbols identified by hard edges (call chain from flow path)
2. Check soft edges for each included symbol
3. If a soft edge points to a symbol with high affinity that isn't already included, add its signature to the context package
4. Soft edge additions are lower priority than hard edge additions — they go in last and get cut first if the token budget is tight

**Key use case:** When no flow path matches. The user says something vague, no path hits, current PIE would hand off to model-driven discovery. With soft edges, the assembler can still find related symbols via type affinity and git co-change, providing some context before the model starts exploring. This upgrades the "no match" fallback from "model figures it out" to "model gets a reasonable starting point."

### How to Test Phase 5

Identify cases in a real project where two pieces of code are clearly related but don't call each other. Verify soft edges connect them. Then test: does including soft-edge-connected code in the context package help the model resolve tasks faster? Compare tool call counts with and without soft edges.

---

## Resolution Log (Replaces Complex Task Memory)

### What to Build

A simple append-only log recording what was fixed and where. Replaces the v1 task memory and context weight system, which was too complex and the model ignored the metadata.

### What to Record After Each Task

```json
{
  "timestamp": "2026-03-06T14:30:00Z",
  "task": "checkout total wrong after discount",
  "matched_path": "cart-total-calculation",
  "path_match_correct": true,
  "fix_location": "DiscountEngine.apply:52",
  "fix_summary": "null check missing on coupon.percentage",
  "files_modified": ["discount.engine.ts"]
}
```

That's it. Plain language. Keyword searchable.

### How It's Used

**For the model:** When a future task matches similar keywords, include one line in the context:
```
Previous related fix: null check missing in DiscountEngine.apply:52
```
The model finds this immediately useful because it reads like a colleague's note.

**For path validation:** The `path_match_correct` field tracks whether the scaffold's path matching was right. If the fix ended up outside the matched path, flag it. Over time this reveals paths that need extending or keywords that need adjusting. This is the only "learning" in the system and it's concrete, debuggable, and inspectable.

### Storage

`.parecode/resolutions.jsonl` — one JSON object per line, append-only. Simple to read, grep, and debug.

### What We're NOT Doing

- No context weights (removed — model ignored them)
- No per-file relevance scoring (too noisy, unclear signal)
- No useful-vs-wasted context analysis (hard to determine reliably)
- No automatic weight adjustment (drifted without clear benefit)

The things that make PIE better over time are deterministic: the graph gets more complete, flow paths get validated by the resolution log, soft edges get more git history to work with. No opaque learning systems.

---

## How It All Fits Together

```
User types a task
       │
       ▼
Signal parsing (existing PIE)
  Extract keywords, file refs, symbol refs from user input
       │
       ▼
Flow path matching (NEW — Phase 2)
  Fuzzy match keywords against path index
       │
       ├── Clear match → Phase 3 (auto context delivery)
       ├── Ambiguous → Ask ONE narrowing question → Phase 3
       └── No match → Soft edge lookup (Phase 5) → Fall back to current PIE
       │
       ▼
Prompt-aware assembly (NEW — Phase 4)
  Check what user already provided
  Only inject what's genuinely new
       │
       ▼
Context package built (zero model calls so far)
  Path context + resolution log note + any soft edge additions
       │
       ▼
Model's first turn
  Receives pre-assembled context
  Goes straight to planning/fixing
  find_symbol still available as fallback, rarely needed
       │
       ▼
Task completes
       │
       ▼
Resolution log append (simple record of what was fixed where)
Path validation (was the matched path correct?)
```

---

## Key Principles to Remember During Implementation

1. **The model never sees infrastructure metadata.** No affinity scores, no soft edge data, no path matching details. The model sees code and plain language notes. Everything else is internal to the scaffold.

2. **The scaffold fills gaps, it doesn't pad.** If the user provided detailed context, back off. If the user was vague, go heavy. Check overlap before injecting.

3. **Always have fallbacks.** No path match → soft edges → current PIE. Narrowing question always has "not sure." Model always has `find_symbol`. Nothing breaks if the new features don't match.

4. **Keep it simple.** Flow paths are lists of symbol IDs. Soft edges are pairs with a score. The resolution log is one JSON line per task. No complex data structures, no opaque learning systems.

5. **Test against real tasks.** The metric is: how many `find_symbol` calls does the model make before it starts planning? Target: zero for 60-70% of tasks on a project with a mature path index.

6. **Build on symbol_index.** Everything references symbols that already exist in the index. Flow paths are ordered lists of symbol IDs. Soft edges connect symbol IDs. No parallel data structures — it's all extensions of what you already have.

---

## Success Criteria

- Flow path tracing produces sensible paths on real projects (manual inspection)
- Path matching correctly identifies the relevant flow for common task descriptions  
- Model discovery tool calls drop 60-70% on tasks where a path matches
- Soft edges connect code that's related but not directly linked (type/git signals)
- Resolution log entries are useful to the model (it references them in its reasoning)
- Prompt-aware assembly scales enrichment appropriately (vague → heavy, detailed → light)
- Everything falls back gracefully — no regressions on tasks that don't match any path
