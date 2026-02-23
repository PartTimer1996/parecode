mod agent;
mod budget;
mod cache;
mod client;
mod config;
mod git;
mod history;
mod hooks;
mod index;
mod init;
mod mcp;
mod plan;
mod sessions;
mod telemetry;
mod tools;
mod tui;
mod ui;

use anyhow::Result;
use clap::Parser;
use config::{ConfigFile, ResolvedConfig};

#[derive(Parser, Debug)]
#[command(
    name = "forge",
    about = "A hyper-efficient coding agent for local and cloud LLMs",
    long_about = None,
)]
struct Args {
    /// Task to run directly (omit to enter interactive TUI mode)
    task: Option<String>,

    /// Profile to use from config file
    #[arg(short, long, env = "FORGE_PROFILE")]
    profile: Option<String>,

    /// Override endpoint URL
    #[arg(long, env = "FORGE_ENDPOINT")]
    endpoint: Option<String>,

    /// Override model name
    #[arg(short, long, env = "FORGE_MODEL")]
    model: Option<String>,

    /// Override API key
    #[arg(long, env = "FORGE_API_KEY")]
    api_key: Option<String>,

    /// Show tool calls without executing them
    #[arg(long)]
    dry_run: bool,

    /// Quick mode — single API call, no multi-turn loop, minimal context
    #[arg(long)]
    quick: bool,

    /// Show extra token / compression detail
    #[arg(short, long)]
    verbose: bool,

    /// Show timestamps on messages
    #[arg(long)]
    timestamps: bool,

    /// Write a default config file to ~/.config/forge/config.toml and exit
    #[arg(long)]
    init: bool,

    /// List available profiles and exit
    #[arg(long)]
    profiles: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── --init ────────────────────────────────────────────────────────────────
    if args.init {
        let path = ConfigFile::write_default_if_missing()?;
        println!("Config written to: {}", path.display());
        println!("Edit it, then run: forge");
        return Ok(());
    }

    let file = ConfigFile::load()?;

    // ── --profiles ────────────────────────────────────────────────────────────
    if args.profiles {
        print_profiles(&file);
        return Ok(());
    }

    let resolved = ResolvedConfig::resolve(
        &file,
        args.profile.as_deref(),
        args.endpoint.as_deref(),
        args.model.as_deref(),
        args.api_key.as_deref(),
    );

    // ── Single-shot mode (non-TUI) ────────────────────────────────────────────
    if let Some(task) = args.task {
        if args.quick {
            run_single_shot_quick(task, resolved, args.verbose).await?;
        } else {
            run_single_shot(task, file, resolved, args.verbose, args.dry_run).await?;
        }
        return Ok(());
    }

    // ── Interactive TUI mode ──────────────────────────────────────────────────
    tui::run(file, resolved, args.verbose, args.dry_run, args.timestamps).await
}

// ── Single-shot mode (plain stdout, no TUI) ───────────────────────────────────

async fn run_single_shot(
    task: String,
    _file: ConfigFile,
    resolved: ResolvedConfig,
    verbose: bool,
    dry_run: bool,
) -> Result<()> {
    use tokio::sync::mpsc;

    println!();
    println!("  ▲ forge  {}  ·  {}", resolved.profile_name, resolved.model);
    println!();
    println!("  task: {task}");
    println!();

    let mut client = client::Client::new(resolved.endpoint.clone(), resolved.model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }
    let mcp = mcp::McpClient::new(&resolved.mcp_servers).await;
    // Single-shot path: read from config (populated by prior TUI run) or detect+write now
    let hook_config = if !resolved.hooks_disabled && resolved.hooks.is_empty() {
        hooks::write_hooks_to_config(&resolved.profile_name)
    } else {
        resolved.hooks.clone()
    };
    let config = agent::AgentConfig {
        verbose,
        dry_run,
        context_tokens: resolved.context_tokens,
        profile_name: resolved.profile_name.clone(),
        model: resolved.model.clone(),
        show_timestamps: false,
        mcp,
        hooks: std::sync::Arc::new(hook_config),
        hooks_enabled: !resolved.hooks_disabled,
        auto_commit: false,
        auto_commit_prefix: String::new(),
        git_context: false,
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<tui::UiEvent>();

    // Spawn agent
    let agent_handle = tokio::spawn(async move {
        agent::run_tui(&task, &client, &config, vec![], None, tx).await
    });

    // Print events to stdout
    while let Some(ev) = rx.recv().await {
        print_event_plain(&ev);
    }

    agent_handle.await??;
    Ok(())
}

fn print_event_plain(ev: &tui::UiEvent) {
    use tui::UiEvent;
    match ev {
        UiEvent::Chunk(c) => {
            print!("{c}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        UiEvent::ThinkingChunk(_) => {} // thinking blocks not shown in plain mode
        UiEvent::ToolCall { name, args_summary } => {
            println!("\n  {} {name} {args_summary}", ui::tool_glyph(name));
        }
        UiEvent::ToolResult { summary } => {
            let first = summary.lines().next().unwrap_or(summary);
            println!("    → {first}");
        }
        UiEvent::CacheHit { path } => {
            println!("    ↩ cache  {path}");
        }
        UiEvent::LoopWarning { tool_name } => {
            println!("  ⚠ loop detected on {tool_name}");
        }
        UiEvent::BudgetWarning => {
            println!("  ⟳ context compressed");
        }
        UiEvent::ToolBudgetHit { limit } => {
            println!("\n  ■ tool call limit ({limit}) reached");
        }
        UiEvent::AgentDone { input_tokens, output_tokens, tool_calls, compressed_count, .. } => {
            let compressed = if *compressed_count > 0 {
                format!("  {compressed_count} outputs truncated")
            } else {
                String::new()
            };
            println!("\n\n  ✓ in {input_tokens} out {output_tokens}  tools {tool_calls}{compressed}");
        }
        UiEvent::AgentError(e) => {
            println!("\n  ✗ {e}");
        }
        UiEvent::TokenStats { input, output, total_input, total_output } => {
            println!("  · i:{input} o:{output} ∑i:{total_input} ∑o:{total_output}");
        }
        UiEvent::ContextUpdate { .. } => {} // skip in plain mode
        UiEvent::HookOutput { event, output, exit_code } => {
            if !output.trim().is_empty() {
                let mark = if *exit_code == 0 { "✓" } else { "✗" };
                println!("  ⚙ {event} {mark}: {}", output.lines().next().unwrap_or(""));
            }
        }
        // Plan and Git lifecycle events only occur in TUI mode — ignore here
        UiEvent::PlanReady(_)
        | UiEvent::PlanGenerateFailed(_)
        | UiEvent::PlanStepStart { .. }
        | UiEvent::PlanStepDone { .. }
        | UiEvent::PlanComplete { .. }
        | UiEvent::PlanFailed { .. }
        | UiEvent::GitChanges { .. }
        | UiEvent::GitAutoCommit { .. }
        | UiEvent::GitError(_) => {}
    }
}

// ── Quick single-shot (plain stdout, no TUI, no loop) ─────────────────────────

async fn run_single_shot_quick(
    task: String,
    resolved: ResolvedConfig,
    verbose: bool,
) -> Result<()> {
    use tokio::sync::mpsc;

    println!();
    println!("  ⚡ forge quick  {}  ·  {}", resolved.profile_name, resolved.model);
    println!();

    let mut client = client::Client::new(resolved.endpoint.clone(), resolved.model.clone());
    if let Some(key) = &resolved.api_key {
        client.set_api_key(key.clone());
    }
    let mcp = mcp::McpClient::new(&resolved.mcp_servers).await;
    let config = agent::AgentConfig {
        verbose,
        dry_run: false,
        context_tokens: resolved.context_tokens,
        profile_name: resolved.profile_name.clone(),
        model: resolved.model.clone(),
        show_timestamps: false,
        mcp,
        hooks: std::sync::Arc::new(hooks::HookConfig::default()),
        hooks_enabled: false,
        auto_commit: false,
        auto_commit_prefix: String::new(),
        git_context: false,
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<tui::UiEvent>();
    let agent_handle = tokio::spawn(async move {
        agent::run_quick(&task, &client, &config, tx).await
    });

    while let Some(ev) = rx.recv().await {
        print_event_plain(&ev);
    }
    agent_handle.await??;
    Ok(())
}

// ── Profiles listing (non-TUI) ────────────────────────────────────────────────

fn print_profiles(file: &ConfigFile) {
    let mut entries: Vec<(String, String, String, u32)> = file
        .profiles
        .iter()
        .map(|(name, p)| (name.clone(), p.endpoint.clone(), p.model.clone(), p.context_tokens))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    println!();
    println!("  Profiles");
    for (name, endpoint, model, ctx) in &entries {
        let marker = if *name == file.default_profile { " ←" } else { "" };
        println!("  {name}{marker}");
        println!("    endpoint  {endpoint}");
        println!("    model     {model}");
        println!("    context   {}k", ctx / 1000);
        println!();
    }
}
