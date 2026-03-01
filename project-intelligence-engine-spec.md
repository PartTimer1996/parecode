# PareCode Project Intelligence Engine (PIE)

## Complete Implementation Specification

**Version:** 1.0
**Status:** Design Complete — Ready for Implementation
**Author:** Ryan (Sonrai Analytics) + Claude
**Date:** February 2026

---

## Executive Summary

The Project Intelligence Engine (PIE) is a persistent, evolving project model that fundamentally changes how agentic coding tools interact with codebases. Instead of treating every task as fresh exploration (the industry standard, costing 50K-100K+ tokens per task), PIE builds a **learning project model** that gets cheaper with every session.

**The core insight:** Every existing coding agent (Claude Code, Cursor, Copilot, OpenCode) treats the model as an explorer — it reads files, forms understanding, reads more files, tries something. This rebuilds understanding from scratch on every task. PIE inverts this: the **scaffold investigates, the model reasons.** The scaffold assembles all context deterministically before the model ever runs, using a persistent project model that compounds knowledge over time.

**Token economics:**
- Session 1 (cold start): ~5K tokens vs ~100K industry standard (20x improvement)
- Session 5 (warm project): ~3K tokens (33x improvement)
- Session 20 (mature project): ~1.5K tokens (60x improvement)

**The key differentiator:** Token usage goes DOWN over time. Every other agent's token usage is static or goes UP as conversations grow. This is PareCode's moat.

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Layer 1: Structural Graph](#2-layer-1-structural-graph)
3. [Layer 2: Project Narrative](#3-layer-2-project-narrative)
4. [Layer 3: Task Memory](#4-layer-3-task-memory)
5. [The Context Assembler](#5-the-context-assembler)
6. [Planning Phase with Discovery](#6-planning-phase-with-discovery)
7. [Execution Phase](#7-execution-phase)
8. [Post-Task Learning Loop](#8-post-task-learning-loop)
9. [Scaling Strategy](#9-scaling-strategy)
10. [Full Lifecycle Walkthrough](#10-full-lifecycle-walkthrough)
11. [Data Structures Reference](#11-data-structures-reference)
12. [File Layout](#12-file-layout)
13. [Language Extractor Interface](#13-language-extractor-interface)
14. [Integration Points with Existing PareCode](#14-integration-points-with-existing-parecode)
15. [Implementation Phases](#15-implementation-phases)

---

## 1. Architecture Overview

### The Three Layers

PIE consists of three layers, each operating at a different timescale:

```
┌─────────────────────────────────────────────────────────────────┐
│  LAYER 1: Structural Graph                                      │
│  Timescale: Rebuilt per file change                              │
│  Cost: Zero tokens (deterministic, tree-sitter powered)         │
│  Contains: Every symbol, edge, cluster in the codebase          │
│  Purpose: The scaffold's understanding of project architecture  │
├─────────────────────────────────────────────────────────────────┤
│  LAYER 2: Project Narrative                                      │
│  Timescale: Generated once, patched incrementally                │
│  Cost: ~2K tokens on first generation, ~500 tokens on update    │
│  Contains: Architecture summary, conventions, hotspots           │
│  Purpose: Natural language project understanding for the model   │
├─────────────────────────────────────────────────────────────────┤
│  LAYER 3: Task Memory                                            │
│  Timescale: Grows with every completed task                      │
│  Cost: Zero tokens (append-only, scaffold queries it)            │
│  Contains: What was done, what worked, what was useful           │
│  Purpose: Learning — context assembly improves over time         │
└─────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────────┐
│  CONTEXT ASSEMBLER                                               │
│  Queries all three layers to build a pre-assembled context       │
│  package for the model. Zero model calls for assembly.           │
│  The model receives a focused, budgeted package and a single     │
│  instruction. No exploration needed.                             │
└─────────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────────┐
│  MODEL (Planner / Executor)                                      │
│  Receives pre-assembled context.                                 │
│  Plans against structure, not raw code.                          │
│  Executes with surgical per-step context.                       │
│  Last resort, not first resort.                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Design Principles

1. **The scaffold investigates, the model reasons.** Everything that CAN be done deterministically MUST be done deterministically. The model fires only when there's an actual decision requiring intelligence.

2. **Token cost proportional to change complexity, not codebase size.** A one-line bug fix in a million-line project should cost roughly the same as a one-line fix in a hundred-line project. The scaffold narrows to the relevant code regardless of project size.

3. **Gets cheaper over time.** The project model compounds knowledge. Session 20 is faster than session 1. This is the opposite of every other agent.

4. **Nothing unbounded reaches the model.** The graph can be 50MB, the task memory can have 500 entries — the model always gets a fixed-budget context package.

---

## 2. Layer 1: Structural Graph

### Purpose

The structural graph is a complete, language-agnostic representation of every symbol, relationship, and structural element in the codebase. It is built entirely by tree-sitter parsing — zero model calls. It answers questions like "what depends on this function?" and "what files form the authentication feature?" without reading any implementation code.

### Data Structures

```rust
struct ProjectGraph {
    files: HashMap<PathBuf, FileEntry>,
    symbols: HashMap<SymbolId, Symbol>,
    edges: Vec<Edge>,
    clusters: Vec<Cluster>,
    // Cache invalidation
    file_hashes: HashMap<PathBuf, String>,
    last_full_index: SystemTime,
}

struct FileEntry {
    path: PathBuf,
    hash: String,              // git blob hash for change detection
    language: Language,
    line_count: usize,
    symbols: Vec<SymbolId>,
    last_indexed: SystemTime,
}

// Language-agnostic symbol representation
// Every language's constructs normalize to these variants
enum Symbol {
    Container {
        id: SymbolId,
        name: String,
        kind: ContainerKind,      // Class, Struct, Module, Enum, Component
        members: Vec<SymbolId>,
        implements: Vec<String>,
        visibility: Visibility,
        file: PathBuf,
        line_range: (usize, usize),
    },
    Callable {
        id: SymbolId,
        name: String,
        params: Vec<Param>,
        return_hint: Option<String>,  // type as string, not resolved
        is_async: bool,
        file: PathBuf,
        line_range: (usize, usize),
    },
    Property {
        id: SymbolId,
        name: String,
        type_hint: Option<String>,
        annotations: Vec<String>,    // decorators, attributes, macros
        file: PathBuf,
        line_range: (usize, usize),
    },
    TypeDef {
        id: SymbolId,
        name: String,
        fields: Vec<(String, String)>,  // (name, type_string)
        file: PathBuf,
        line_range: (usize, usize),
    },
    Import {
        id: SymbolId,
        source: String,
        symbols: Vec<String>,
        file: PathBuf,
        line: usize,
    },
}

struct Param {
    name: String,
    type_hint: Option<String>,
    default: bool,
}

enum ContainerKind { Class, Struct, Module, Enum, Component, Trait, Interface }
enum Visibility { Public, Private, Protected, Internal }

enum Edge {
    Imports { from: SymbolId, to: SymbolId },
    Calls { from: SymbolId, to: SymbolId },
    Implements { from: SymbolId, trait_or_interface: String },
    Contains { parent: SymbolId, child: SymbolId },
    Uses { from: SymbolId, to: SymbolId },     // type references
}

// Functional groupings detected by graph analysis
struct Cluster {
    name: String,
    files: Vec<PathBuf>,
    entry_points: Vec<SymbolId>,  // most-referenced symbols in cluster
    internal_edges: usize,        // edges within cluster
    external_edges: usize,        // edges to other clusters
    description: String,          // generated once by model
}
```

### Graph Construction

```rust
fn build_graph(root: &Path) -> ProjectGraph {
    let mut graph = ProjectGraph::new();

    // Pass 1: Parse all files, extract symbols
    for file in walk_source_files(root) {
        let lang = detect_language(&file);       // by file extension
        let extractor = get_extractor(lang);      // trait implementation per language
        let source = fs::read(&file).unwrap();
        let tree = extractor.parse(&source);      // tree-sitter, ~1ms per file

        let symbols = extractor.extract_symbols(&tree, &source);
        let imports = extractor.extract_imports(&tree, &source);

        graph.add_file(FileEntry {
            path: file.clone(),
            hash: git_blob_hash(&file),
            language: lang,
            line_count: source.lines().count(),
            symbols: symbols.iter().map(|s| s.id()).collect(),
            last_indexed: SystemTime::now(),
        });

        for symbol in symbols {
            graph.add_symbol(symbol);
        }
        for import in imports {
            graph.add_edge(Edge::Imports {
                from: file_symbol_id(&file),
                to: resolve_import(&import, root),
            });
        }
    }

    // Pass 2: Resolve call edges
    // Walk method bodies looking for references to known symbols
    graph.resolve_call_edges();

    // Pass 3: Detect clusters via community detection
    graph.clusters = detect_clusters(&graph);

    graph
}
```

### Cluster Detection Algorithm

Clusters are detected using a simplified community detection approach on the dependency graph:

```rust
fn detect_clusters(graph: &ProjectGraph) -> Vec<Cluster> {
    // 1. Build adjacency matrix from Import and Calls edges
    // 2. Run label propagation or Louvain-style community detection
    //    - Each file starts as its own community
    //    - Iteratively merge files that share the most edges
    //    - Stop when modularity gain is below threshold
    // 3. Name clusters by most common directory path or dominant symbol names
    //    e.g., files in auth/ with Auth* symbols → "authentication"
    // 4. Identify entry points: symbols with most incoming edges from outside cluster

    // Simplified approach that works well in practice:
    // Group by directory first (src/auth/, src/cart/, etc.)
    // Then merge groups that have >3 cross-imports
    // Split groups that have clear internal boundaries
}
```

### Incremental Updates

The graph is NOT rebuilt from scratch on every run:

```rust
fn update_graph(graph: &mut ProjectGraph, root: &Path) {
    // 1. Get changed files since last index
    let changed = get_changed_files(root, &graph.file_hashes);
    // Uses: git diff --name-only against stored hashes
    // Fallback: compare file modification times

    if changed.is_empty() {
        return; // Nothing to do
    }

    // 2. Re-parse only changed files
    for file in &changed {
        // Remove old symbols and edges for this file
        graph.remove_file_data(file);
        // Re-extract
        let lang = detect_language(file);
        let extractor = get_extractor(lang);
        let source = fs::read(file).unwrap();
        let tree = extractor.parse(&source);
        // ... same as initial build, but only for changed files
    }

    // 3. Re-resolve edges that touch changed files
    graph.resolve_call_edges_for(&changed);

    // 4. Recheck cluster boundaries
    // Only if new import edges cross existing cluster boundaries
    if graph.has_cross_cluster_changes(&changed) {
        graph.clusters = detect_clusters(graph);
    }

    // 5. Update hashes
    for file in &changed {
        graph.file_hashes.insert(file.clone(), git_blob_hash(file));
    }
}
```

**Performance targets:**
- Full index of 1,000 files: ~2-3 seconds
- Incremental update of 5 changed files: ~50ms
- Graph serialization/deserialization: ~100ms

### Structural Map Generation

When the planner needs to see a project overview or file structure, the graph generates a **structural map** — a compact text representation that costs a fraction of actual code:

```rust
impl ProjectGraph {
    fn structural_map_for_cluster(&self, cluster_name: &str) -> String {
        // Produces something like:
        //
        // ## authentication (4 files)
        //   auth.service.ts (89 lines)
        //     class AuthService
        //       refreshToken(token: string) → Observable<Token>  [async]
        //       validateSession() → Observable<boolean>
        //       logout() → void
        //     injects: [HttpClient, TokenStorage]
        //     called_by: [AuthInterceptor, LoginComponent]
        //
        //   auth.interceptor.ts (67 lines)
        //     class AuthInterceptor implements HttpInterceptor
        //       intercept(req, next) → Observable<HttpEvent>
        //     calls: [AuthService.refreshToken]
        //
        // ~100-150 tokens for an entire cluster vs ~3000+ tokens for raw code
    }

    fn structural_map_for_file(&self, path: &Path) -> String {
        // Same but for a single file
        // ~30-50 tokens per file
    }

    fn excerpt_symbol(&self, symbol_id: &SymbolId) -> Option<String> {
        // Extracts the actual source code for a specific symbol
        // Reads from disk, returns only the line range for that symbol
        // Used by context assembler for targeted reads
    }
}
```

---

## 3. Layer 2: Project Narrative

### Purpose

The project narrative is a natural language understanding of the project that serves as compressed, reusable context for the model. It replaces the thousands of tokens an agent would normally spend "understanding" the project on each task.

### Data Structures

```rust
struct ProjectNarrative {
    // One-time generation from structural graph
    // ~100 tokens, captures overall architecture
    architecture_summary: String,

    // Per-cluster summaries, ~30 tokens each
    cluster_summaries: HashMap<String, String>,

    // Discovered conventions
    conventions: Vec<Convention>,

    // Known risk areas
    hotspots: Vec<Hotspot>,

    // Maintenance tracking
    last_synthesized: SystemTime,
    patches: Vec<NarrativePatch>,
}

struct Convention {
    pattern: String,           // human-readable: "Services return Observable<T>"
    evidence: Vec<PathBuf>,    // files demonstrating this pattern
    confidence: f32,           // 0.0 to 1.0, based on consistency
}

struct Hotspot {
    symbol: SymbolId,
    reason: String,           // "400 lines, 12 dependents, 3 past bugs"
    risk_score: f32,          // composite score
}

struct NarrativePatch {
    timestamp: SystemTime,
    cluster: String,
    update: String,           // "Migrated UserComponent to standalone"
}
```

### Convention Detection (Zero Model Calls)

Conventions are discovered by pattern matching on the structural graph:

```rust
fn detect_conventions(graph: &ProjectGraph) -> Vec<Convention> {
    let mut conventions = vec![];

    // Pattern: What do methods in services return?
    let service_files = graph.files_matching("*.service.*");
    let return_types = service_files.iter()
        .flat_map(|f| graph.callables_in(f))
        .filter_map(|c| c.return_hint.as_ref())
        .collect::<Vec<_>>();
    let (most_common, frequency) = most_frequent(&return_types);
    if frequency > 0.7 {
        conventions.push(Convention {
            pattern: format!("Services return {}", most_common),
            evidence: service_files,
            confidence: frequency,
        });
    }

    // Pattern: Do components have test files?
    let components = graph.files_matching("*.component.*")
        .filter(|f| !f.ends_with(".spec.*"));
    let with_tests = components.iter()
        .filter(|c| graph.file_exists(&c.with_extension("spec.ts")))
        .count();
    let coverage = with_tests as f32 / components.len() as f32;
    if coverage > 0.5 {
        conventions.push(Convention {
            pattern: format!("Components have unit tests ({}% coverage)", (coverage * 100.0) as usize),
            evidence: components.collect(),
            confidence: coverage,
        });
    }

    // Pattern: Change detection strategy
    // Pattern: State management approach
    // Pattern: Error handling patterns
    // Pattern: Async patterns (Observable vs Promise vs async/await)
    // ... more patterns detectable from structural data

    conventions
}
```

### Narrative Generation (One Model Call)

The architecture summary is the ONLY part that requires a model call during project init:

```rust
fn generate_narrative(
    graph: &ProjectGraph,
    clusters: &[Cluster],
    conventions: &[Convention],
    model: &dyn LlmClient,
) -> ProjectNarrative {
    // Build the input for the model — entirely from graph data
    let prompt = format!(
        "You are analyzing a codebase structure. Generate a concise architectural summary (100 words max).\n\n\
        Project stats: {} files, {} symbols across {} languages\n\
        Framework detection: {}\n\
        Clusters:\n{}\n\
        Conventions:\n{}\n\n\
        Produce:\n1. architecture_summary (100 words)\n2. One summary per cluster (30 words each)",
        graph.files.len(),
        graph.symbols.len(),
        graph.languages().join(", "),
        detect_framework(graph),
        clusters.iter().map(|c| format!(
            "  - {} ({} files, entry points: {})",
            c.name,
            c.files.len(),
            c.entry_points.iter().take(3).map(|s| graph.symbol_name(s)).collect::<Vec<_>>().join(", ")
        )).collect::<Vec<_>>().join("\n"),
        conventions.iter().map(|c| format!(
            "  - {} (confidence: {:.0}%)", c.pattern, c.confidence * 100.0
        )).collect::<Vec<_>>().join("\n"),
    );

    // ~1500 tokens in, ~200 tokens out
    let response = model.call_once(&prompt);
    let parsed = parse_narrative_response(&response);

    ProjectNarrative {
        architecture_summary: parsed.summary,
        cluster_summaries: parsed.cluster_summaries,
        conventions,
        hotspots: detect_hotspots(graph),
        last_synthesized: SystemTime::now(),
        patches: vec![],
    }
}
```

### Hotspot Detection (Zero Model Calls)

```rust
fn detect_hotspots(graph: &ProjectGraph) -> Vec<Hotspot> {
    graph.symbols.values()
        .filter_map(|symbol| {
            let dependents = graph.incoming_edges(symbol.id()).len();
            let line_count = symbol.line_range().map(|(s, e)| e - s).unwrap_or(0);
            let complexity = estimate_cyclomatic_complexity(symbol); // from tree-sitter

            // Composite risk score
            let risk = (dependents as f32 * 0.4)
                + (line_count as f32 / 100.0 * 0.3)
                + (complexity as f32 / 10.0 * 0.3);

            if risk > 2.0 {
                Some(Hotspot {
                    symbol: symbol.id(),
                    reason: format!(
                        "{} lines, {} dependents, complexity {}",
                        line_count, dependents, complexity
                    ),
                    risk_score: risk,
                })
            } else {
                None
            }
        })
        .collect()
}
```

### Incremental Narrative Updates

After each task, the narrative gets a lightweight patch:

```rust
fn update_narrative_after_task(
    narrative: &mut ProjectNarrative,
    task: &CompletedTask,
    graph: &ProjectGraph,
) {
    // Add a patch for the affected cluster
    let affected_cluster = graph.cluster_for_files(&task.files_modified);
    narrative.patches.push(NarrativePatch {
        timestamp: SystemTime::now(),
        cluster: affected_cluster.clone(),
        update: task.outcome.summary().to_string(),
    });

    // If patches are accumulating (>10 for a cluster), re-synthesize
    // that cluster's summary with one cheap model call
    let cluster_patches: Vec<_> = narrative.patches.iter()
        .filter(|p| p.cluster == affected_cluster)
        .collect();

    if cluster_patches.len() > 10 {
        let resynthesized = resynthesize_cluster_summary(
            &narrative.cluster_summaries[&affected_cluster],
            &cluster_patches,
        );
        narrative.cluster_summaries.insert(affected_cluster.clone(), resynthesized);
        narrative.patches.retain(|p| p.cluster != affected_cluster);
    }

    // Re-detect conventions if structural changes were significant
    // (e.g., new patterns introduced, existing patterns broken)
    // This is a zero-model-call operation
    narrative.conventions = detect_conventions(graph);
}
```

---

## 4. Layer 3: Task Memory

### Purpose

Task memory records every completed task with full provenance — what was asked, what context was assembled, what worked, what didn't. This enables the context assembler to learn and improve over time, making future tasks cheaper.

### Data Structures

```rust
struct TaskMemory {
    tasks: Vec<CompletedTask>,
    // Pre-computed indexes for fast lookup
    by_cluster: HashMap<String, Vec<usize>>,
    by_file: HashMap<PathBuf, Vec<usize>>,
    by_keyword: HashMap<String, Vec<usize>>,
    // Aggregated summaries for old tasks
    cluster_summaries: HashMap<String, ClusterTaskSummary>,
}

struct CompletedTask {
    id: String,
    timestamp: SystemTime,

    // What was asked
    description: String,

    // What the scaffold discovered during context assembly
    context_assembly_log: ContextAssemblyLog,

    // What happened
    files_modified: Vec<PathBuf>,
    symbols_modified: Vec<SymbolId>,

    // Outcome
    outcome: TaskOutcome,

    // Learning data: what context was actually useful?
    useful_context: Vec<ContextSource>,
    wasted_context: Vec<ContextSource>,

    // Token accounting
    tokens_planning: usize,
    tokens_execution: usize,
    tokens_wasted: usize,
}

enum TaskOutcome {
    Solved {
        first_attempt: bool,
        summary: String,
    },
    PartiallyResolved {
        summary: String,
        remaining: String,
    },
    Failed {
        reason: String,
        what_was_tried: Vec<String>,
    },
}

enum ContextSource {
    FileExcerpt(PathBuf, (usize, usize)),      // file, line range
    SymbolExcerpt(SymbolId),
    TestOutput(String),
    GitHistory(PathBuf),
    PastTask(String),                           // task ID
    ClusterSummary(String),
    Convention(String),
    ArchitectureSummary,
}

struct ContextAssemblyLog {
    signals_detected: Vec<Signal>,
    clusters_identified: Vec<String>,
    similar_tasks_found: Vec<String>,
    symbols_traced: Vec<SymbolId>,
    total_tokens_assembled: usize,
    assembly_time_ms: u64,
}

struct ClusterTaskSummary {
    total_tasks: usize,
    common_files: Vec<(PathBuf, usize)>,         // file, frequency
    common_patterns: Vec<String>,                  // "token refresh issues", "type errors"
    average_tokens: usize,
    last_task: SystemTime,
}
```

### Task Memory Querying

```rust
impl TaskMemory {
    fn find_similar(
        &self,
        task_description: &str,
        clusters: &[String],
        signals: &[Signal],
    ) -> Vec<&CompletedTask> {
        // Step 1: Narrow by cluster (fast, uses index)
        let cluster_candidates: HashSet<usize> = clusters.iter()
            .flat_map(|c| self.by_cluster.get(c).unwrap_or(&vec![]))
            .copied()
            .collect();

        // Step 2: Narrow by file overlap if signals point to specific files
        let file_candidates: HashSet<usize> = signals.iter()
            .filter_map(|s| s.file_path())
            .flat_map(|f| self.by_file.get(f).unwrap_or(&vec![]))
            .copied()
            .collect();

        // Step 3: Union candidates, score by relevance
        let all_candidates: HashSet<usize> = cluster_candidates
            .union(&file_candidates)
            .copied()
            .collect();

        let mut scored: Vec<(f32, &CompletedTask)> = all_candidates.iter()
            .map(|&idx| {
                let task = &self.tasks[idx];
                let score =
                    keyword_overlap(task_description, &task.description) * 0.4
                    + file_overlap_score(&task.files_modified, signals) * 0.3
                    + recency_score(task.timestamp) * 0.2
                    + success_score(&task.outcome) * 0.1;
                (score, task)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        scored.into_iter()
            .take(3)                    // Max 3 similar tasks
            .map(|(_, task)| task)
            .collect()
    }
}
```

### Task Memory Compaction

Old tasks get compacted into cluster-level summaries to prevent unbounded growth:

```rust
impl TaskMemory {
    fn compact(&mut self, graph: &ProjectGraph) {
        let cutoff = SystemTime::now() - Duration::from_secs(180 * 86400); // 6 months

        let (old, recent): (Vec<_>, Vec<_>) = self.tasks
            .drain(..)
            .partition(|t| t.timestamp < cutoff);

        // Aggregate old tasks by cluster
        for cluster_name in graph.cluster_names() {
            let cluster_tasks: Vec<_> = old.iter()
                .filter(|t| t.files_modified.iter()
                    .any(|f| graph.file_cluster(f) == Some(&cluster_name)))
                .collect();

            if !cluster_tasks.is_empty() {
                self.cluster_summaries.insert(
                    cluster_name.clone(),
                    ClusterTaskSummary {
                        total_tasks: cluster_tasks.len(),
                        common_files: most_frequent_files(&cluster_tasks, 5),
                        common_patterns: extract_common_patterns(&cluster_tasks),
                        average_tokens: average_token_usage(&cluster_tasks),
                        last_task: cluster_tasks.iter()
                            .map(|t| t.timestamp)
                            .max()
                            .unwrap(),
                    },
                );
            }
        }

        self.tasks = recent;
        self.rebuild_indexes();
    }
}
```

---

## 5. The Context Assembler

### Purpose

The context assembler is the central intelligence of PIE. It queries all three layers to build a pre-assembled context package for the model — **zero model calls during assembly.** The model receives a focused, budgeted package and a single instruction.

### Signal Parsing

Before assembly, the scaffold parses the user's input for actionable signals:

```rust
enum Signal {
    StackTrace(Vec<StackFrame>),
    TestFailure { file: PathBuf, line: usize, message: String },
    CompilerError { file: PathBuf, line: usize, message: String },
    FileReference(PathBuf),
    SymbolReference(String),
    UserDescription(String),
    GitDiff(String),
    PastedError(String),
}

struct StackFrame {
    file: PathBuf,
    line: usize,
    function: Option<String>,
}

enum SignalStrength {
    Strong,    // Stack trace, test failures with file:line
    Medium,    // File references, symbol mentions, vague errors
    Weak,      // Pure description, no concrete pointers
}

fn parse_signals(input: &str) -> Vec<Signal> {
    let mut signals = vec![];

    // Detect stack traces (file:line patterns)
    for capture in STACK_TRACE_REGEX.captures_iter(input) {
        signals.push(Signal::StackTrace(parse_stack_frames(&capture)));
    }

    // Detect test failure output
    for capture in TEST_FAILURE_REGEX.captures_iter(input) {
        signals.push(Signal::TestFailure {
            file: PathBuf::from(&capture["file"]),
            line: capture["line"].parse().unwrap(),
            message: capture["message"].to_string(),
        });
    }

    // Detect compiler errors
    for capture in COMPILER_ERROR_REGEX.captures_iter(input) {
        signals.push(Signal::CompilerError {
            file: PathBuf::from(&capture["file"]),
            line: capture["line"].parse().unwrap(),
            message: capture["message"].to_string(),
        });
    }

    // Detect file path references
    for capture in FILE_PATH_REGEX.captures_iter(input) {
        signals.push(Signal::FileReference(PathBuf::from(&capture[0])));
    }

    // Everything else is a description
    if signals.is_empty() || has_description_beyond_signals(input, &signals) {
        signals.push(Signal::UserDescription(input.to_string()));
    }

    signals
}
```

### Signal Enrichment (Zero Model Calls)

If the user provides a vague description with no concrete signals, the scaffold enriches by running tests/compiler:

```rust
fn enrich_signals(
    signals: &mut Vec<Signal>,
    graph: &ProjectGraph,
) {
    if signal_strength(signals) != SignalStrength::Weak {
        return; // Already have concrete signals, don't waste time
    }

    // Auto-detect and run the project's test command
    if let Some(test_cmd) = detect_test_command(graph) {
        // detect_test_command checks: Cargo.toml → "cargo test"
        //                             package.json → "npm test"
        //                             pytest.ini → "pytest"
        //                             etc.
        let output = run_command_with_timeout(&test_cmd, Duration::from_secs(30));
        if let Some(failures) = parse_test_failures(&output) {
            signals.extend(failures.into_iter().map(Signal::TestFailure));
        }
    }

    // Auto-detect and run the compiler/type checker
    if let Some(build_cmd) = detect_build_command(graph) {
        let output = run_command_with_timeout(&build_cmd, Duration::from_secs(30));
        if let Some(errors) = parse_compiler_errors(&output) {
            signals.extend(errors.into_iter().map(Signal::CompilerError));
        }
    }
}
```

### The Assembly Algorithm

```rust
struct ContextPackage {
    architecture: String,                   // ~100 tokens, always included
    cluster_context: Vec<(String, String)>, // cluster name + summary, ~30 tokens each
    conventions: Vec<String>,               // relevant conventions, ~20 tokens each
    code_excerpts: Vec<CodeExcerpt>,        // surgical code extractions
    related_past_tasks: Vec<TaskSummary>,   // 1-3 similar past tasks, ~50 tokens each
    signals: Vec<Signal>,                   // parsed signals from user input
    total_tokens: usize,
}

struct CodeExcerpt {
    file: PathBuf,
    symbol: Option<SymbolId>,
    line_range: (usize, usize),
    code: String,
    token_count: usize,
    reason: String,           // why this was included — for learning
}

impl ContextAssembler {
    fn assemble(
        &self,
        task: &str,
        signals: &[Signal],
        token_budget: usize,     // default: ~2000 tokens for planning context
    ) -> ContextPackage {
        let mut package = ContextPackage::new();
        let mut remaining_budget = token_budget;

        // === ALWAYS INCLUDED (fixed cost) ===

        // Architecture summary (~100 tokens)
        package.architecture = self.narrative.architecture_summary.clone();
        remaining_budget -= count_tokens(&package.architecture);

        // === STEP 1: Identify relevant clusters ===
        let clusters = self.identify_clusters(task, signals);
        // Method: check which clusters contain files referenced in signals,
        // or keyword-match cluster names against task description
        for cluster in &clusters {
            if let Some(summary) = self.narrative.cluster_summaries.get(cluster) {
                package.cluster_context.push((cluster.clone(), summary.clone()));
                remaining_budget -= count_tokens(summary);
            }
        }

        // Relevant conventions for identified clusters (~20 tokens each)
        let conventions = self.narrative.conventions_for_clusters(&clusters);
        for conv in conventions.iter().take(3) {
            package.conventions.push(conv.pattern.clone());
            remaining_budget -= count_tokens(&conv.pattern);
        }

        // === STEP 2: Check task memory ===
        let similar_tasks = self.memory.find_similar(task, &clusters, signals);
        for past_task in similar_tasks.iter().take(3) {
            let summary = past_task.one_line_summary();
            package.related_past_tasks.push(summary.clone());
            remaining_budget -= count_tokens(&summary.text);
        }

        // === STEP 3: Signal-driven targeted reads ===
        let mut traced_symbols: Vec<SymbolId> = vec![];

        for signal in signals {
            match signal {
                Signal::TestFailure { file, line, .. } |
                Signal::CompilerError { file, line, .. } => {
                    // Find the symbol at this location in the graph
                    if let Some(symbol_id) = self.graph.symbol_at(file, *line) {
                        traced_symbols.push(symbol_id);
                        // Read 15 lines around the signal point
                        let excerpt = self.read_around(file, *line, 7);
                        if excerpt.token_count <= remaining_budget {
                            remaining_budget -= excerpt.token_count;
                            package.code_excerpts.push(excerpt);
                        }
                    }
                }
                Signal::StackTrace(frames) => {
                    // Top 3 frames only
                    for frame in frames.iter().take(3) {
                        if let Some(symbol_id) = self.graph.symbol_at(&frame.file, frame.line) {
                            traced_symbols.push(symbol_id);
                        }
                        let excerpt = self.read_around(&frame.file, frame.line, 5);
                        if excerpt.token_count <= remaining_budget {
                            remaining_budget -= excerpt.token_count;
                            package.code_excerpts.push(excerpt);
                        }
                    }
                }
                Signal::FileReference(file) => {
                    // Include structural map for referenced file (cheap)
                    let map = self.graph.structural_map_for_file(file);
                    remaining_budget -= count_tokens(&map);
                    // Don't read code yet — let planner request specific symbols
                }
                _ => {}
            }
        }

        // === STEP 4: Dependency expansion ===
        // For traced symbols, include symbols they call (1 level deep)
        for symbol_id in &traced_symbols {
            let callees = self.graph.outgoing_calls(symbol_id);
            for callee in callees.iter().take(3) {
                // Include callee's excerpt if budget allows
                let excerpt = self.excerpt_symbol(callee);
                if excerpt.token_count <= remaining_budget {
                    remaining_budget -= excerpt.token_count;
                    package.code_excerpts.push(excerpt);
                }
            }
        }

        // === STEP 5: History-guided expansion ===
        // If similar past tasks used context we haven't included yet, add it
        for past_task in &similar_tasks {
            for ctx in &past_task.useful_context {
                if !package.already_covers(ctx) && remaining_budget > 100 {
                    if let Some(excerpt) = self.resolve_context_source(ctx) {
                        if excerpt.token_count <= remaining_budget {
                            remaining_budget -= excerpt.token_count;
                            package.code_excerpts.push(excerpt);
                        }
                    }
                }
            }
        }

        // === STEP 6: Vague task fallback ===
        // If we still have very little code context, read cluster entry points
        if package.code_excerpts.is_empty() && remaining_budget > 500 {
            for cluster in &clusters {
                let entry_points = self.graph.cluster_entry_points(cluster);
                for ep in entry_points.iter().take(3) {
                    let excerpt = self.excerpt_symbol(ep);
                    if excerpt.token_count <= remaining_budget {
                        remaining_budget -= excerpt.token_count;
                        package.code_excerpts.push(excerpt);
                    }
                }
            }
        }

        package.signals = signals.to_vec();
        package.total_tokens = token_budget - remaining_budget;
        package
    }
}
```

---

## 6. Planning Phase with Discovery

### Purpose

The planner receives the pre-assembled context package and produces a structured plan. For most tasks on warm projects, the package contains everything needed. For vague or complex tasks, the planner can request additional symbol reads — but with strict guardrails.

### Planner Mode Selection

```rust
enum PlannerMode {
    Sufficient,
    Discovery {
        reads_remaining: usize,
        tokens_remaining: usize,
    },
}

fn select_planner_mode(context: &ContextPackage) -> PlannerMode {
    match context.signal_strength() {
        SignalStrength::Strong => PlannerMode::Sufficient,
        // Concrete signals + code excerpts + past tasks = enough

        SignalStrength::Medium => PlannerMode::Discovery {
            reads_remaining: 2,
            tokens_remaining: 800,
        },
        // Some signals but might need to check related code

        SignalStrength::Weak => PlannerMode::Discovery {
            reads_remaining: 4,
            tokens_remaining: 1500,
        },
        // Vague description — model might need to explore
    }
}
```

### Planning Tools

The planner gets TWO tools, not general file access:

```rust
// Tool 1: Read a specific symbol's code (costs a read, costs tokens)
struct ReadSymbol {
    symbol_name: String,  // e.g., "AuthService.refreshToken"
}
// Scaffold resolves to exact file + line range, returns code excerpt

// Tool 2: List symbols in a file (FREE — just graph data)
struct ListSymbols {
    file_path: String,    // e.g., "src/auth/auth.service.ts"
}
// Returns structural map — function signatures, types, no implementation
// Model can browse structure freely, then request specific symbols
```

**Critical:** The planner CANNOT read whole files. It can list what's in a file (free) and read specific symbols (budgeted). This forces surgical exploration.

### Planning Loop

```rust
fn planning_phase(
    context: &ContextPackage,
    model: &dyn LlmClient,
    graph: &ProjectGraph,
) -> Plan {
    let mut mode = select_planner_mode(context);

    // Build the planning prompt from the context package
    let mut prompt = build_planning_prompt(context);
    // This renders:
    //   ARCHITECTURE: [architecture_summary]
    //   CLUSTER: [relevant cluster summaries]
    //   CONVENTIONS: [relevant conventions]
    //   RELATED PAST WORK: [similar task summaries]
    //   SIGNALS: [test failures, errors, stack traces]
    //   CODE: [pre-assembled excerpts]
    //   ---
    //   Produce a structured plan as JSON. If you need to see more code,
    //   use read_symbol(name) or list_symbols(file). Budget: N reads remaining.

    let tools = match &mode {
        PlannerMode::Sufficient => vec![],
        PlannerMode::Discovery { .. } => vec![
            Tool::ReadSymbol,
            Tool::ListSymbols,
        ],
    };

    loop {
        let response = model.call(&prompt, &tools);

        match response {
            Response::Plan(plan) => {
                return plan; // Model produced a plan, done
            }

            Response::ToolCall(ToolCall::ReadSymbol { symbol_name }) => {
                if let PlannerMode::Discovery { reads_remaining, tokens_remaining } = &mut mode {
                    if *reads_remaining == 0 || *tokens_remaining == 0 {
                        prompt.push_str(
                            "\nRead budget exhausted. Produce a plan with current context."
                        );
                        // Remove tools to force plan output
                        tools.clear();
                        continue;
                    }

                    if let Some(excerpt) = graph.excerpt_symbol_by_name(&symbol_name) {
                        let cost = count_tokens(&excerpt);
                        if cost <= *tokens_remaining {
                            prompt.push_str(&format!("\n--- {} ---\n{}\n", symbol_name, excerpt));
                            *reads_remaining -= 1;
                            *tokens_remaining -= cost;
                        } else {
                            let truncated = truncate_to_budget(&excerpt, *tokens_remaining);
                            prompt.push_str(&truncated);
                            *tokens_remaining = 0;
                        }
                    } else {
                        prompt.push_str(&format!(
                            "\nSymbol '{}' not found in project graph.\n", symbol_name
                        ));
                    }
                }
            }

            Response::ToolCall(ToolCall::ListSymbols { file_path }) => {
                // FREE — no budget cost, just graph data
                let map = graph.structural_map_for_file(&PathBuf::from(&file_path));
                prompt.push_str(&format!("\n--- Structure of {} ---\n{}\n", file_path, map));
            }
        }
    }
}
```

### Plan Output Structure

The planner produces a plan that references symbols, not files:

```rust
struct Plan {
    analysis: String,              // model's understanding of the problem
    steps: Vec<PlanStep>,
    estimated_tokens: usize,       // scaffold calculates from step count
}

struct PlanStep {
    id: usize,
    description: String,
    symbols_needed: Vec<String>,   // symbol names, resolved at execution time
    verification: StepVerification,
    tool_budget: usize,            // max tool calls for this step (default: 3)
}

enum StepVerification {
    None,
    BuildSuccess,
    TestPass(String),              // specific test command
    PatternAbsent(String, String), // file, pattern that should be gone
    FileChanged(String),
    CommandSuccess(String),
}
```

### Plan Cost Estimation

Before user approval, the scaffold estimates total cost:

```rust
fn estimate_plan_cost(plan: &Plan, graph: &ProjectGraph) -> CostEstimate {
    let mut total_tokens = 0;

    for step in &plan.steps {
        // Estimate context size for this step
        let symbol_tokens: usize = step.symbols_needed.iter()
            .filter_map(|name| graph.symbol_line_count(name))
            .map(|lines| lines * 4) // rough: 4 tokens per line
            .sum();

        // Conventions + instruction overhead
        let overhead = 200;

        // Response estimate
        let response_estimate = 400;

        total_tokens += symbol_tokens + overhead + response_estimate;
    }

    CostEstimate {
        planning_tokens: current_planning_usage,
        execution_tokens: total_tokens,
        total_tokens: current_planning_usage + total_tokens,
        estimated_cost: calculate_api_cost(total_tokens),
    }
}
```

---

## 7. Execution Phase

### Purpose

Each plan step executes with a **fresh context window** containing ONLY the code needed for that step. The executor never sees the big picture — it gets a bounded instruction and surgical code.

### Step Execution

```rust
fn execute_plan(
    plan: &Plan,
    graph: &mut ProjectGraph,
    model: &dyn LlmClient,
) -> Vec<StepResult> {
    let mut results = vec![];

    for step in &plan.steps {
        // === CRITICAL: Resolve symbols NOW, not at plan time ===
        // Previous steps may have modified files, shifting line numbers
        let fresh_excerpts: Vec<CodeExcerpt> = step.symbols_needed.iter()
            .filter_map(|name| graph.excerpt_symbol_by_name(name))
            .collect();

        // Build minimal executor context
        let exec_prompt = format!(
            "INSTRUCTION: {}\n\
            CONVENTIONS: {}\n\
            \n\
            CODE:\n{}\n\
            \n\
            Produce an edit.",
            step.description,
            relevant_conventions_for_symbols(&step.symbols_needed),
            fresh_excerpts.iter()
                .map(|e| format!("--- {} (lines {}-{}) ---\n{}", 
                    e.file.display(), e.line_range.0, e.line_range.1, e.code))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        // Typical executor context: ~400-800 tokens
        // Compare: traditional agents send 10-30K tokens per step

        let response = model.call_once(&exec_prompt);
        let edit = parse_edit(&response);

        // Apply the edit
        match apply_edit(&edit) {
            Ok(()) => {
                // Update graph for modified files (incremental, ~10ms)
                graph.reindex_files(&edit.files_modified());

                // Run verification
                let verification = run_verification(&step.verification);

                if verification.failed() {
                    // ONE retry with error context
                    let retry_prompt = format!(
                        "Your edit produced this error:\n{}\n\n\
                        Current code after your edit:\n{}\n\n\
                        Fix the error.",
                        verification.error_output,
                        read_current_code(&edit.files_modified()),
                    );
                    let retry_response = model.call_once(&retry_prompt);
                    let retry_edit = parse_edit(&retry_response);
                    apply_edit(&retry_edit)?;
                    graph.reindex_files(&retry_edit.files_modified());
                }

                // Git checkpoint
                git_commit_checkpoint(&format!("parecode: {}", step.description));

                results.push(StepResult::Success {
                    step_id: step.id,
                    files_modified: edit.files_modified(),
                    tokens_used: count_tokens(&exec_prompt) + count_tokens(&response),
                });
            }
            Err(e) => {
                results.push(StepResult::Failed {
                    step_id: step.id,
                    error: e.to_string(),
                });
                // Don't continue plan on failure — let user decide
                break;
            }
        }
    }

    results
}
```

---

## 8. Post-Task Learning Loop

### Purpose

After every completed task, PIE updates all three layers and records what context was useful vs wasted. This is how the system gets cheaper over time.

### The Update Cycle

```rust
fn post_task_update(
    model: &mut ProjectModel,
    task_description: &str,
    plan: &Plan,
    results: &[StepResult],
    context_package: &ContextPackage,
) {
    // === 1. Graph Update (deterministic, ~50ms) ===
    let modified_files: Vec<_> = results.iter()
        .filter_map(|r| match r {
            StepResult::Success { files_modified, .. } => Some(files_modified),
            _ => None,
        })
        .flatten()
        .collect();
    model.graph.reindex_files(&modified_files);

    // Recheck cluster boundaries if imports changed
    if model.graph.has_cross_cluster_changes(&modified_files) {
        model.graph.clusters = detect_clusters(&model.graph);
    }

    // === 2. Narrative Patch (deterministic or one cheap model call) ===
    let outcome_summary = summarize_outcome(results);
    update_narrative_after_task(&mut model.narrative, &outcome_summary, &model.graph);

    // === 3. Task Memory Append (deterministic, instant) ===
    let (useful, wasted) = analyze_context_usage(context_package, plan, results);
    // Compare: what was in the context package vs what the model actually
    // referenced in its edits. Symbols that appeared in the package but
    // were never mentioned in the model's reasoning = wasted context.

    model.memory.append(CompletedTask {
        id: generate_task_id(),
        timestamp: SystemTime::now(),
        description: task_description.to_string(),
        context_assembly_log: context_package.assembly_log.clone(),
        files_modified: modified_files.clone(),
        symbols_modified: extract_modified_symbols(results, &model.graph),
        outcome: build_outcome(results),
        useful_context: useful,
        wasted_context: wasted,
        tokens_planning: context_package.total_tokens + plan.planning_tokens,
        tokens_execution: results.iter().map(|r| r.tokens_used()).sum(),
        tokens_wasted: wasted.iter().map(|w| w.token_cost()).sum(),
    });

    // === 4. Context Weight Adjustment (deterministic, instant) ===
    // If certain files/symbols are frequently wasted, reduce their
    // relevance score for future context assembly
    for ctx in &wasted {
        model.context_weights.decrease(ctx, 0.1);
    }
    for ctx in &useful {
        model.context_weights.increase(ctx, 0.1);
    }

    // === 5. Hotspot Recalculation (deterministic) ===
    // Files that keep appearing in tasks get higher hotspot scores
    model.narrative.hotspots = detect_hotspots_with_history(
        &model.graph,
        &model.memory,
    );

    // === 6. Persist ===
    model.save();
}
```

### Context Usage Analysis

```rust
fn analyze_context_usage(
    package: &ContextPackage,
    plan: &Plan,
    results: &[StepResult],
) -> (Vec<ContextSource>, Vec<ContextSource>) {
    let mut useful = vec![];
    let mut wasted = vec![];

    // Check which code excerpts the model actually used
    // by looking at which files/symbols appear in the plan steps
    // and which files were modified during execution
    let referenced_files: HashSet<_> = plan.steps.iter()
        .flat_map(|s| &s.symbols_needed)
        .filter_map(|name| symbol_to_file(name))
        .collect();

    let modified_files: HashSet<_> = results.iter()
        .filter_map(|r| r.files_modified())
        .flatten()
        .collect();

    let all_relevant = referenced_files.union(&modified_files).collect::<HashSet<_>>();

    for excerpt in &package.code_excerpts {
        if all_relevant.contains(&excerpt.file) {
            useful.push(ContextSource::FileExcerpt(
                excerpt.file.clone(),
                excerpt.line_range,
            ));
        } else {
            wasted.push(ContextSource::FileExcerpt(
                excerpt.file.clone(),
                excerpt.line_range,
            ));
        }
    }

    // Past tasks that influenced the plan are useful
    for past_task in &package.related_past_tasks {
        if plan.analysis.contains(&past_task.description_keywords()) {
            useful.push(ContextSource::PastTask(past_task.id.clone()));
        }
    }

    (useful, wasted)
}
```

---

## 9. Scaling Strategy

### What Grows and What Doesn't

| Component | Growth | 1K files | 10K files | 100K files | Notes |
|-----------|--------|----------|-----------|------------|-------|
| Structural graph | O(files + edges) | ~500KB | ~5MB | ~50MB | On disk only, never in model context raw |
| Cluster map | O(clusters) | ~2KB | ~10KB | ~50KB | Typically 5-20 clusters regardless |
| Conventions | O(patterns) | ~1KB | ~2KB | ~5KB | Bounded by pattern types, not files |
| Narrative | O(clusters) | ~2KB | ~5KB | ~15KB | Fixed per cluster |
| Task memory | O(tasks) | Grows over time | | | Compacted after 6 months |
| Context weights | O(files) | ~50KB | ~500KB | ~5MB | On disk, queried selectively |

### What the Model Actually Sees (Always Bounded)

Regardless of project size, the model receives:

```
Architecture summary:        ~100 tokens  (fixed)
Cluster summaries (2-3):     ~60-90 tokens (fixed per cluster)
Conventions (2-3):           ~40-60 tokens (fixed)
Past task summaries (1-3):   ~50-150 tokens (fixed per task)
Code excerpts:               ~400-1200 tokens (budgeted)
Signals:                     ~50-100 tokens (from user input)
────────────────────────────────────────────────
Total planning context:      ~700-1700 tokens

Executor per-step context:   ~400-800 tokens (just the relevant symbol)
```

A 100-file project and a 100,000-file project produce similar-sized context packages because the scaffold narrows to the relevant cluster and symbols before anything reaches the model.

### Task Memory Scaling

```
Tasks 1-50:    Full detail retained, precise matching
Tasks 51-200:  Full detail retained, index-based lookup
Tasks 200+:    Tasks >6 months old compacted into cluster summaries
               Recent tasks always retained in full
               Cluster summaries: ~30 tokens per cluster
               Replaces potentially hundreds of individual records
```

---

## 10. Full Lifecycle Walkthrough

### Cold Start: First Time in a New Repo

```
User runs: parecode
Time: T+0

1. Detect project root (.git)                          [0ms]
2. Check for .parecode/ directory                      [0ms, not found]
3. Walk source files, detect languages                 [50ms]
4. Run tree-sitter extractors on all files             [2000ms for ~500 files]
5. Build symbol index and edges                        [200ms]
6. Detect clusters via community detection             [100ms]
7. Detect conventions via pattern matching             [200ms]
8. Detect hotspots via graph analysis                  [50ms]
9. Generate narrative (ONE model call, ~2K tokens)     [3000ms]
10. Serialize and save to .parecode/                   [100ms]

Total init time: ~6 seconds
Model calls: 1
Tokens used: ~2,000

User types: "fix the login timeout bug"
Time: T+6s

11. Parse signals from input                           [1ms]
12. No concrete signals → run tests automatically      [5000ms]
13. Parse test failures → 2 failures found             [10ms]
14. Assemble context package:
    a. Architecture summary (from cache)               [0ms, 100 tokens]
    b. Match signals to clusters → "authentication"    [5ms, 30 tokens]
    c. Search task memory → empty (first session)      [1ms, 0 tokens]
    d. Read around failure points                      [10ms, ~300 tokens]
    e. Expand dependencies (1 level)                   [5ms, ~200 tokens]
    f. Package total                                   [~630 tokens]
15. Select planner mode: Medium (2 discovery reads)    [0ms]
16. Model plans (ONE model call, ~1.5K tokens)         [2000ms]
    - Model requests 1 additional symbol read          [+400 tokens]
    - Model produces 2-step plan
17. Show plan to user, get approval                    [user time]
18. Execute step 1 (ONE model call, ~800 tokens)       [1500ms]
19. Verify step 1 (run build)                          [3000ms]
20. Execute step 2 (ONE model call, ~800 tokens)       [1500ms]
21. Verify step 2 (run tests)                          [5000ms]
22. Post-task learning:
    a. Reindex modified files                          [20ms]
    b. Patch narrative                                 [0ms]
    c. Append task memory                              [1ms]
    d. Analyze context usage                           [5ms]
    e. Persist                                         [50ms]

Total task time: ~25 seconds (excluding user review)
Model calls: 4 (1 narrative + 1 plan + 2 execute)
Tokens used: ~5,100
```

### Warm Project: 20th Session

```
User runs: parecode
Time: T+0

1. Detect project root                                 [0ms]
2. Load .parecode/ from disk                           [100ms]
3. Incremental graph update (git diff → 3 changed)     [30ms]

Total init time: ~130ms
Model calls: 0
Tokens used: 0

User types: "the cart total is wrong when applying discount codes"
Time: T+0.13s

4. Parse signals → UserDescription only (weak)          [1ms]
5. Run tests → 1 failure in cart.spec.ts                [5000ms]
6. Assemble context package:
    a. Architecture summary (cached)                   [0ms, 100 tokens]
    b. Match to "cart" cluster                         [5ms, 30 tokens]
    c. Search task memory → Task #14 (discount fix)    [5ms, 50 tokens]
       Task #14 useful_context included:
       CartService.applyDiscount, DiscountValidator
    d. Read around test failure                        [5ms, ~200 tokens]
    e. History-guided: also read CartService.applyDiscount
       (was useful last time)                          [5ms, ~150 tokens]
    f. Context weights say: skip CartRenderer
       (was wasted last 2 times it was included)       [0ms, saved ~100 tokens]
    g. Package total                                   [~530 tokens]
7. Select planner mode: Strong (test failure = concrete) [0ms]
8. Model plans (ONE call, ~1K tokens)                  [1500ms]
   - No discovery reads needed
   - 1-step plan (simple fix)
9. Execute step 1 (ONE call, ~600 tokens)              [1000ms]
10. Verify (run tests)                                 [5000ms]
11. Post-task learning                                 [30ms]

Total task time: ~12 seconds
Model calls: 2 (1 plan + 1 execute)
Tokens used: ~1,600
```

The improvement comes from:
- No narrative generation (cached from session 1)
- Task memory found similar past work → tighter context
- Context weights learned to exclude CartRenderer → fewer wasted tokens
- Strong signals from test → no discovery reads needed
- Simple fix → 1 step instead of 2

---

## 11. Data Structures Reference

### Complete SymbolId

```rust
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
struct SymbolId {
    file: PathBuf,
    name: String,
    kind: SymbolKind,
}

enum SymbolKind {
    Container,
    Callable,
    Property,
    TypeDef,
    Import,
}
```

### Context Weights

```rust
struct ContextWeights {
    // Per-file relevance adjustment
    file_weights: HashMap<PathBuf, f32>,
    // Per-symbol relevance adjustment
    symbol_weights: HashMap<SymbolId, f32>,
    // Default weight is 1.0
    // Increased for frequently useful context
    // Decreased for frequently wasted context
    // Clamped to [0.1, 3.0] range
}

impl ContextWeights {
    fn adjust_relevance(&self, excerpt: &CodeExcerpt) -> f32 {
        let file_w = self.file_weights.get(&excerpt.file).unwrap_or(&1.0);
        let symbol_w = excerpt.symbol
            .as_ref()
            .and_then(|s| self.symbol_weights.get(s))
            .unwrap_or(&1.0);
        file_w * symbol_w
    }
}
```

### Language Enum

```rust
enum Language {
    TypeScript,
    JavaScript,
    Rust,
    Python,
    Go,
    Java,
    CSharp,
    Ruby,
    Cpp,
    C,
    Unknown(String),
}

impl Language {
    fn from_extension(ext: &str) -> Language {
        match ext {
            "ts" | "tsx" => Language::TypeScript,
            "js" | "jsx" | "mjs" => Language::JavaScript,
            "rs" => Language::Rust,
            "py" => Language::Python,
            "go" => Language::Go,
            "java" => Language::Java,
            "cs" => Language::CSharp,
            "rb" => Language::Ruby,
            "cpp" | "cc" | "cxx" => Language::Cpp,
            "c" | "h" => Language::C,
            other => Language::Unknown(other.to_string()),
        }
    }
}
```

---

## 12. File Layout

```
.parecode/
├── project.graph              # Serialized ProjectGraph (bincode for speed)
├── project.narrative.json     # ProjectNarrative (JSON, human-readable)
├── task_memory.jsonl          # Append-only task log (one JSON per line)
├── conventions.json           # Discovered conventions
├── hotspots.json              # Risk areas with scores
├── clusters.json              # Functional groupings
├── context_weights.json       # Learned relevance weights
├── extractors/                # Language-specific configuration overrides
│   ├── typescript.toml        # e.g., custom Angular patterns to detect
│   └── rust.toml
└── config.toml                # PIE configuration
    # - token_budget_planning: 2000
    # - token_budget_executor: 1000
    # - max_discovery_reads: 4
    # - compaction_age_days: 180
    # - cluster_detection: "label_propagation"
```

All files in `.parecode/` are **derived data** — deletable and rebuildable:
- Graph rebuilds from source in seconds
- Narrative needs one model call to regenerate
- Task memory is the only truly accumulated value (append-only, backed by git)

---

## 13. Language Extractor Interface

### Trait Definition

```rust
trait LanguageExtractor: Send + Sync {
    /// File extensions this extractor handles
    fn extensions(&self) -> &[&str];

    /// Parse source into a tree-sitter Tree
    fn parse(&self, source: &[u8]) -> Tree;

    /// Extract all symbols (containers, callables, properties, types)
    fn extract_symbols(&self, tree: &Tree, source: &[u8]) -> Vec<Symbol>;

    /// Extract all imports/dependencies
    fn extract_imports(&self, tree: &Tree, source: &[u8]) -> Vec<Symbol>;

    /// Extract call relationships from function bodies
    fn extract_calls(&self, tree: &Tree, source: &[u8]) -> Vec<(String, String)>;
    // Returns: (caller_name, callee_name) pairs

    /// Detect framework-specific patterns (optional)
    fn detect_patterns(&self, tree: &Tree, source: &[u8]) -> Vec<DetectedPattern> {
        vec![] // default: no special patterns
    }
}

struct DetectedPattern {
    kind: String,           // "angular_component", "react_hook", "express_route"
    symbol: String,         // which symbol this applies to
    metadata: HashMap<String, String>, // pattern-specific data
}
```

### Example: TypeScript Extractor (Skeleton)

```rust
struct TypeScriptExtractor {
    parser: tree_sitter::Parser,
}

impl LanguageExtractor for TypeScriptExtractor {
    fn extensions(&self) -> &[&str] {
        &["ts", "tsx"]
    }

    fn extract_symbols(&self, tree: &Tree, source: &[u8]) -> Vec<Symbol> {
        let mut symbols = vec![];

        // tree-sitter query for classes
        let class_query = Query::new(tree_sitter_typescript::language(), r#"
            (class_declaration
                name: (type_identifier) @class_name
                (class_heritage
                    (implements_clause
                        (type_identifier) @implements))?
                body: (class_body) @body)
        "#).unwrap();

        // tree-sitter query for functions
        let fn_query = Query::new(tree_sitter_typescript::language(), r#"
            (function_declaration
                name: (identifier) @fn_name
                parameters: (formal_parameters) @params
                return_type: (type_annotation)? @return_type)
        "#).unwrap();

        // tree-sitter query for arrow functions assigned to variables
        let arrow_query = Query::new(tree_sitter_typescript::language(), r#"
            (lexical_declaration
                (variable_declarator
                    name: (identifier) @fn_name
                    value: (arrow_function
                        parameters: (formal_parameters) @params)))
        "#).unwrap();

        // ... execute queries, normalize to Symbol enum

        symbols
    }

    fn detect_patterns(&self, tree: &Tree, source: &[u8]) -> Vec<DetectedPattern> {
        // Angular-specific: detect @Component, @Injectable, @Input, @Output
        // React-specific: detect useState, useEffect, custom hooks
        // Express-specific: detect router.get/post/put/delete
        vec![]
    }
}
```

### Fallback Extractor

For languages without a dedicated extractor:

```rust
struct FallbackExtractor;

impl LanguageExtractor for FallbackExtractor {
    fn extensions(&self) -> &[&str] { &[] }

    fn extract_symbols(&self, _tree: &Tree, source: &[u8]) -> Vec<Symbol> {
        // Regex-based extraction — less accurate but works for any language
        // Detects: function/def/fn/func + name
        //          class/struct/type + name
        //          import/require/use/include + source
        // Produces Symbol entries with approximate line ranges
        regex_extract_symbols(source)
    }

    fn extract_imports(&self, _tree: &Tree, source: &[u8]) -> Vec<Symbol> {
        regex_extract_imports(source)
    }

    fn extract_calls(&self, _tree: &Tree, _source: &[u8]) -> Vec<(String, String)> {
        vec![] // Can't reliably detect calls without language-specific parsing
    }
}
```

This means ANY file gets at least basic structural indexing. Languages with dedicated extractors get richer graphs.

### Priority for Extractor Implementation

1. **TypeScript/JavaScript** — most common web development
2. **Rust** — PareCode itself, and growing user base
3. **Python** — massive user base
4. **Go** — common backend language
5. Community contributions for the rest

---

## 14. Integration Points with Existing PareCode

PIE integrates with PareCode's existing architecture at these points:

### 1. Session Initialization

**Current:** PareCode loads config, connects to model provider, starts REPL.
**With PIE:** Add `ProjectModel::load_or_init()` before REPL starts. Graph build happens here.

### 2. Task Classification

**Current:** PareCode's `classifier.rs` determines if a task is mechanical or needs the model.
**With PIE:** Classifier also consults the graph and conventions. More tasks become mechanical when the scaffold knows the project patterns.

### 3. Context Management

**Current:** PareCode's `context.rs` manages token budget within a conversation.
**With PIE:** Context assembler replaces manual context management. Budget is enforced before the model is called, not during the conversation.

### 4. Planning

**Current:** PareCode's `planner.rs` generates plans from user input.
**With PIE:** Planner receives pre-assembled context packages instead of building context itself. Discovery reads are the new mechanism for planner exploration.

### 5. Execution

**Current:** PareCode's `executor.rs` runs plan steps with fresh context per step.
**With PIE:** Executor gets symbol-resolved excerpts instead of file-path-based excerpts. Late binding means correct line ranges even after previous steps modify files.

### 6. Verification

**Current:** PareCode's `verifier.rs` runs build/test after steps.
**With PIE:** Verification feeds back into task memory. Test failures and build errors become signals for future context assembly.

### 7. Hooks

**Current:** PareCode auto-detects Cargo.toml/package.json and runs appropriate commands.
**With PIE:** Hook output becomes signals. Compiler errors from hooks feed directly into context assembly.

### 8. Telemetry

**Current:** PareCode's `.parecode/telemetry.jsonl` tracks token usage.
**With PIE:** Task memory subsumes telemetry. Richer data: not just token count but context usage analysis.

---

## 15. Implementation Phases

### Phase 1: Structural Graph (Foundation)

**Goal:** Build and persist the project graph. Ship the `ListSymbols` structural map as a user-visible feature (e.g., `/map` command in REPL).

**Tasks:**
- Implement `LanguageExtractor` trait
- Build TypeScript extractor (primary language)
- Build Rust extractor (dogfooding)
- Build fallback regex extractor
- Implement `ProjectGraph` construction and serialization
- Implement incremental updates via git diff
- Implement cluster detection
- Add `/map` command to REPL that shows structural overview
- Add `/map <file>` to show file-level structure

**Validation:** Run on PareCode's own codebase. Verify graph accuracy against manual inspection. Measure build time on 1K+ file projects.

### Phase 2: Project Narrative + Context Assembler

**Goal:** Replace model-driven exploration with scaffold-driven context assembly.

**Tasks:**
- Implement convention detection
- Implement hotspot detection
- Implement narrative generation (one model call)
- Implement signal parsing (stack traces, test failures, errors)
- Implement signal enrichment (auto-run tests/compiler)
- Implement `ContextAssembler` with all assembly steps
- Wire context assembler into planner (replace current context building)

**Validation:** Compare token usage before/after on the same set of tasks. Target: 5x reduction in planning-phase tokens.

### Phase 3: Task Memory + Learning

**Goal:** Implement the learning loop. Token usage should decrease measurably across sessions.

**Tasks:**
- Implement `TaskMemory` with append and indexing
- Implement `CompletedTask` recording after each task
- Implement context usage analysis (useful vs wasted)
- Implement context weights adjustment
- Implement task memory querying (similar task lookup)
- Wire history-guided expansion into context assembler
- Implement task memory compaction

**Validation:** Run 20+ tasks on a test project. Measure token usage trend. Confirm decreasing cost per task.

### Phase 4: Discovery Mode + Polish

**Goal:** Add planner discovery reads. Polish the full PIE system.

**Tasks:**
- Implement `ReadSymbol` and `ListSymbols` tools for planner
- Implement `PlannerMode` selection based on signal strength
- Implement discovery budget enforcement
- Add PIE stats to PareCode's stats bar (e.g., "PIE: 47 tasks learned, 12 clusters")
- Add `/pie status` command showing project model health
- Add `/pie reset` to rebuild from scratch
- Add `/pie explain` to show why certain context was assembled
- Documentation and README updates

**Validation:** End-to-end benchmarks against Claude Code and Cursor on identical tasks. Measure token usage, success rate, and task completion time.

---

## Appendix: Key Design Decisions

### Why tree-sitter over LSP?

Tree-sitter is zero-config, works offline, parses any file regardless of project state, and is fast enough for real-time use. LSP requires a running language server, project configuration, and can be slow to start. Tree-sitter gives us 80% of the structural information at 1% of the setup cost. LSP integration can be added later as an optional enhancement for deeper type-level analysis.

### Why not embeddings/RAG?

Embeddings require an embedding model (resource cost on 16GB machines), produce fuzzy matches (might miss structurally important but semantically distant code), and don't capture dependency relationships. The structural graph captures exact relationships at zero token cost. Embeddings could be added later as a complement for semantic search within the graph, but the graph is the foundation.

### Why append-only task memory?

Append-only is simple, safe, and git-friendly. Compaction handles growth. The alternative — updating a database — adds complexity without clear benefit. JSONL is human-readable, greppable, and trivially parseable.

### Why not cache model responses?

Model responses are task-specific and rarely reusable verbatim. The value is in the *structural* learning (which files are relevant, which patterns recur, what context is wasted) — not in caching specific model outputs.

---

## Summary

The Project Intelligence Engine transforms PareCode from a smart single-task agent into a **learning system** that compounds project knowledge over time. The core innovation is inverting the industry assumption that models should explore codebases. Instead, the scaffold investigates deterministically and the model reasons with surgical, pre-assembled context.

**The result: token usage that decreases with every session, making PareCode fundamentally more efficient than any agent that treats every task as fresh exploration.**
