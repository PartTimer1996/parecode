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
mod setup;
mod telemetry;
mod tools;
mod tui;
mod ui;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use config::{ConfigFile, ResolvedConfig};

#[derive(Parser, Debug)]
#[command(
    name = "parecode",
    about = "A hyper-efficient coding agent for local and cloud LLMs",
    long_about = None,
)]
struct Args {
    /// Task to run directly (omit to enter interactive TUI mode)
    task: Option<String>,

    /// Profile to use from config file
    #[arg(short, long, env = "PARECODE_PROFILE")]
    profile: Option<String>,

    /// Override endpoint URL
    #[arg(long, env = "PARECODE_ENDPOINT")]
    endpoint: Option<String>,

    /// Override model name
    #[arg(short, long, env = "PARECODE_MODEL")]
    model: Option<String>,

    /// Override API key
    #[arg(long, env = "PARECODE_API_KEY")]
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

    /// Write a default config file to ~/.config/parecode/config.toml and exit
    #[arg(long)]
    init: bool,

    /// List available profiles and exit
    #[arg(long)]
    profiles: bool,

    /// Generate shell completions and print to stdout (bash, zsh, fish, elvish)
    #[arg(long, value_name = "SHELL")]
    completions: Option<String>,

    /// Update parecode to the latest release
    #[arg(long)]
    update: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── --init ────────────────────────────────────────────────────────────────
    if args.init {
        let path = ConfigFile::write_default_if_missing()?;
        println!("Config written to: {}", path.display());
        println!("Edit it, then run: parecode");
        return Ok(());
    }

    // ── --completions ─────────────────────────────────────────────────────────
    if let Some(shell_name) = &args.completions {
        return generate_completions(shell_name);
    }

    // ── --update ──────────────────────────────────────────────────────────────
    if args.update {
        return self_update().await;
    }

    // ── First-run wizard ──────────────────────────────────────────────────────
    // If no config file exists and no CLI/env overrides fully configure us,
    // run the interactive setup wizard before loading config.
    if !config::config_path().exists()
        && args.endpoint.is_none()
        && args.model.is_none()
    {
        match setup::run_setup_wizard().await {
            Ok(true) => {
                // Wizard wrote a config — show completion hint once
                if let Some(hint) = setup::shell_completion_hint() {
                    println!();
                    println!("{hint}");
                }
                println!();
            }
            Ok(false) => {
                // User skipped — they can use env vars or --init later
            }
            Err(e) => {
                eprintln!("  Setup wizard error: {e}");
                eprintln!("  Falling back to defaults. Run `parecode --init` to configure.");
            }
        }
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
    // Check for updates in the background (non-blocking)
    let update_notice = tokio::spawn(async { setup::check_for_update().await });

    tui::run(file, resolved, args.verbose, args.dry_run, args.timestamps, update_notice).await
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
    println!("  ▲ parecode  {}  ·  {}", resolved.profile_name, resolved.model);
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
        _profile_name: resolved.profile_name.clone(),
        _model: resolved.model.clone(),
        _show_timestamps: false,
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

    // Print events to stdout, tracking accumulated token usage
    let mut accum_input: u32 = 0;
    let mut accum_output: u32 = 0;
    while let Some(ev) = rx.recv().await {
        print_event_plain(&ev, &mut accum_input, &mut accum_output);
    }

    agent_handle.await??;
    Ok(())
}

fn print_event_plain(ev: &tui::UiEvent, accum_input: &mut u32, accum_output: &mut u32) {
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
            if *accum_input > 0 || *accum_output > 0 {
                println!("\n  ✗ {e}  (partial: in {accum_input} out {accum_output})");
            } else {
                println!("\n  ✗ {e}");
            }
        }
        UiEvent::TokenStats { _input: _, _output: _, total_input, total_output, .. } => {
            *accum_input = *total_input;
            *accum_output = *total_output;
        }
        UiEvent::ContextUpdate { .. } => {} // skip in plain mode
        UiEvent::HookOutput { event, output, exit_code } => {
            if !output.trim().is_empty() {
                let mark = if *exit_code == 0 { "✓" } else { "✗" };
                println!("  ⚙ {event} {mark}: {}", output.lines().next().unwrap_or(""));
            }
        }
        // Plan, Git, and AskUser lifecycle events only occur in TUI mode — ignore here
        UiEvent::PlanReady(_)
        | UiEvent::PlanGenerateFailed(_)
        | UiEvent::PlanStepStart { .. }
        | UiEvent::PlanStepDone { .. }
        | UiEvent::PlanComplete { .. }
        | UiEvent::PlanFailed { .. }
        | UiEvent::GitChanges { .. }
        | UiEvent::GitAutoCommit { .. }
        | UiEvent::GitError(_)
        | UiEvent::AskUser { .. } => {}
        UiEvent::SystemMsg(msg) => {
            println!("  {msg}");
        }
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
    println!("  ⚡ parecode quick  {}  ·  {}", resolved.profile_name, resolved.model);
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
        _profile_name: resolved.profile_name.clone(),
        _model: resolved.model.clone(),
        _show_timestamps: false,
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

    let mut accum_input: u32 = 0;
    let mut accum_output: u32 = 0;
    while let Some(ev) = rx.recv().await {
        print_event_plain(&ev, &mut accum_input, &mut accum_output);
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

// ── Shell completions ─────────────────────────────────────────────────────────

fn generate_completions(shell_name: &str) -> Result<()> {
    use clap_complete::{Shell, generate};

    let shell: Shell = match shell_name.to_lowercase().as_str() {
        "bash"    => Shell::Bash,
        "zsh"     => Shell::Zsh,
        "fish"    => Shell::Fish,
        "elvish"  => Shell::Elvish,
        _ => {
            eprintln!("Unknown shell: {shell_name}");
            eprintln!("Supported: bash, zsh, fish, elvish");
            std::process::exit(1);
        }
    };

    let mut cmd = Args::command();
    generate(shell, &mut cmd, "parecode", &mut std::io::stdout());
    Ok(())
}

// ── Self-update ───────────────────────────────────────────────────────────────

async fn self_update() -> Result<()> {
    use std::io::Write;

    let current = env!("CARGO_PKG_VERSION");
    print!("  Checking for updates... ");
    std::io::stdout().flush()?;

    // Query GitHub directly (bypass cache — user explicitly asked for update)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let release_url = "https://api.github.com/repos/PartTimer1996/parecode/releases/latest";
    let resp = client
        .get(release_url)
        .header("User-Agent", format!("parecode/{current}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            println!("✗");
            eprintln!("  Failed to reach GitHub: {e}");
            eprintln!("  Check manually: https://github.com/PartTimer1996/parecode/releases/latest");
            std::process::exit(1);
        }
    };

    if !resp.status().is_success() {
        println!("✗");
        eprintln!("  GitHub API returned HTTP {}", resp.status());
        std::process::exit(1);
    }

    let body: serde_json::Value = resp.json().await?;
    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No tag_name in release response"))?;
    let latest = tag.trim_start_matches('v').to_string();

    if !setup::version_newer(&latest, current) {
        println!("parecode {current} is already the latest version.");
        return Ok(());
    }

    println!("parecode {current} → {latest} available");
    println!();

    // Determine platform target
    let target = detect_target();
    let Some(target) = target else {
        eprintln!("  ✗ Could not detect platform. Update manually from:");
        eprintln!("    https://github.com/PartTimer1996/parecode/releases/latest");
        std::process::exit(1);
    };

    // Fetch release assets — reuse the body we already have
    let assets = body["assets"].as_array().ok_or_else(|| {
        anyhow::anyhow!("No assets in release")
    })?;

    print!("  Downloading parecode {latest} for {target}... ");
    std::io::stdout().flush()?;

    // Find the matching archive asset
    // cargo-dist names: parecode-{target}.tar.xz (unix) or .zip (windows)
    let is_windows = target.contains("windows");
    let unix_exts = [".tar.xz", ".tar.gz"];

    let asset_url = if is_windows {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            if name.contains(&target) && name.ends_with(".zip") {
                a["browser_download_url"].as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
    } else {
        // Try .tar.xz first (cargo-dist default), then .tar.gz fallback
        unix_exts.iter().find_map(|ext| {
            assets.iter().find_map(|a| {
                let name = a["name"].as_str()?;
                if name.contains(&target) && name.ends_with(ext) {
                    a["browser_download_url"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
    };

    let Some(download_url) = asset_url else {
        println!("✗");
        eprintln!("  No matching asset for target {target}");
        eprintln!("  Check: https://github.com/PartTimer1996/parecode/releases/latest");
        std::process::exit(1);
    };

    // Download archive
    let resp = client
        .get(&download_url)
        .header("User-Agent", format!("parecode/{current}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        println!("✗");
        eprintln!("  Download failed (HTTP {})", resp.status());
        std::process::exit(1);
    }

    let bytes = resp.bytes().await?;
    println!("✓ ({:.1} MB)", bytes.len() as f64 / 1_048_576.0);

    // Extract binary from archive
    print!("  Extracting... ");
    std::io::stdout().flush()?;

    let binary_name = if is_windows { "parecode.exe" } else { "parecode" };
    let extracted = if is_windows {
        extract_from_zip(&bytes, binary_name)?
    } else if download_url.ends_with(".tar.xz") {
        extract_from_tar_xz(&bytes, binary_name)?
    } else {
        extract_from_tar_gz(&bytes, binary_name)?
    };
    println!("✓");

    // Replace self
    let current_exe = std::env::current_exe()?;
    print!("  Replacing {}... ", current_exe.display());
    std::io::stdout().flush()?;

    replace_exe(&current_exe, &extracted)?;
    println!("✓");

    println!();
    println!("  parecode {latest} installed.");
    Ok(())
}

/// Detect the cargo-dist target triple for the current platform.
fn detect_target() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (arch, os) {
        ("x86_64",  "linux")   => Some("x86_64-unknown-linux-musl".to_string()),
        ("aarch64", "linux")   => Some("aarch64-unknown-linux-musl".to_string()),
        ("x86_64",  "macos")   => Some("x86_64-apple-darwin".to_string()),
        ("aarch64", "macos")   => Some("aarch64-apple-darwin".to_string()),
        ("x86_64",  "windows") => Some("x86_64-pc-windows-msvc".to_string()),
        _ => None,
    }
}

/// Extract a named file from a .tar.xz archive in memory.
fn extract_from_tar_xz(data: &[u8], target_name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let decoder = xz2::read::XzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if let Some(fname) = path.file_name() {
            if fname == target_name {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
    }
    anyhow::bail!("Binary '{target_name}' not found in archive")
}

/// Extract a named file from a .tar.gz archive in memory.
fn extract_from_tar_gz(data: &[u8], target_name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if let Some(fname) = path.file_name() {
            if fname == target_name {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
    }
    anyhow::bail!("Binary '{target_name}' not found in archive")
}

/// Extract a named file from a .zip archive in memory.
fn extract_from_zip(data: &[u8], target_name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let reader = std::io::Cursor::new(data);
    let mut zip = zip::ZipArchive::new(reader)?;

    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let path = std::path::Path::new(file.name());
        if let Some(fname) = path.file_name() {
            if fname == target_name {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
    }
    anyhow::bail!("Binary '{target_name}' not found in zip")
}

/// Replace the running executable with new binary data.
/// Uses rename-swap pattern for atomic replacement on all platforms.
fn replace_exe(current_path: &std::path::Path, new_data: &[u8]) -> Result<()> {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let dir = current_path.parent().unwrap_or(std::path::Path::new("."));
    let backup = dir.join("parecode.old");
    let staging = dir.join("parecode.new");

    // Write new binary to staging file
    fs::write(&staging, new_data)?;

    // Set executable permissions on unix
    #[cfg(unix)]
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;

    // Move current → backup, staging → current
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    fs::rename(current_path, &backup)?;
    if let Err(e) = fs::rename(&staging, current_path) {
        // Roll back
        let _ = fs::rename(&backup, current_path);
        return Err(e.into());
    }

    // Clean up backup
    let _ = fs::remove_file(&backup);

    Ok(())
}
