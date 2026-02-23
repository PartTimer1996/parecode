use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// ── MCP server config ─────────────────────────────────────────────────────────

/// Configuration for a single MCP server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Human-readable name, used as the server prefix in tool names ("brave.brave_web_search")
    pub name: String,
    /// Command + args to spawn the server (e.g. ["npx", "-y", "@modelcontextprotocol/server-brave-search"])
    pub command: Vec<String>,
    /// Optional environment variables to inject (e.g. {"BRAVE_API_KEY": "..."})
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

// ── Profile ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// OpenAI-compatible endpoint URL
    pub endpoint: String,
    /// Model identifier
    pub model: String,
    /// Context window size in tokens. Used for proactive budget enforcement.
    #[serde(default = "default_context_tokens")]
    pub context_tokens: u32,
    /// Optional API key (sent as Bearer token)
    pub api_key: Option<String>,
    /// Optional separate model for plan generation.
    /// If set, PareCode uses this model to think/plan and `model` for step execution.
    /// Example: planner_model = "claude-opus-4-6" with model = "claude-haiku-4-5"
    #[serde(default)]
    pub planner_model: Option<String>,
    /// MCP servers to connect for this profile
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Optional: cost per 1M input tokens in USD, used for plan cost estimates.
    /// Example: 0.25 for Haiku, 3.0 for Sonnet, 15.0 for Opus.
    pub cost_per_mtok_input: Option<f64>,
    /// Explicit hook commands per event. If all empty, auto-detection kicks in.
    #[serde(default)]
    pub hooks: crate::hooks::HookConfig,
    /// Set to true to disable all hooks for this profile, including auto-detected ones.
    #[serde(default)]
    pub hooks_disabled: bool,
    /// Auto-commit all changes after each successful task. Default: false.
    #[serde(default)]
    pub auto_commit: bool,
    /// Prefix for auto-commit messages. Default: "parecode: ".
    #[serde(default = "default_auto_commit_prefix")]
    pub auto_commit_prefix: String,
    /// Inject `git status` into system prompt and create checkpoints before tasks.
    /// Set to false to disable all git integration. Default: true.
    #[serde(default = "default_git_context")]
    pub git_context: bool,
}

fn default_context_tokens() -> u32 {
    32_768
}

fn default_auto_commit_prefix() -> String {
    "parecode: ".to_string()
}

fn default_git_context() -> bool {
    true
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434".to_string(),
            model: "qwen3:14b".to_string(),
            context_tokens: default_context_tokens(),
            api_key: None,
            planner_model: None,
            mcp_servers: Vec::new(),
            cost_per_mtok_input: None,
            hooks: crate::hooks::HookConfig::default(),
            hooks_disabled: false,
            auto_commit: false,
            auto_commit_prefix: default_auto_commit_prefix(),
            git_context: default_git_context(),
        }
    }
}

// ── Config file ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    /// Which profile to use when none is specified
    #[serde(default = "default_profile_name")]
    pub default_profile: String,

    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

fn default_profile_name() -> String {
    "default".to_string()
}

impl ConfigFile {
    /// Load from disk, or return a default config if the file doesn't exist yet.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file at {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file at {}", path.display()))
    }

    /// Write a starter config file to disk (only if it doesn't exist).
    pub fn write_default_if_missing() -> Result<PathBuf> {
        let path = config_path();
        if path.exists() {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, DEFAULT_CONFIG_TOML)?;
        Ok(path)
    }

    /// Resolve the active profile given an optional override name.
    pub fn resolve_profile(&self, name: Option<&str>) -> Option<&Profile> {
        let key = name.unwrap_or(&self.default_profile);
        self.profiles.get(key)
    }

}

// ── Resolved runtime config (after merging file + CLI overrides) ──────────────

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub endpoint: String,
    pub model: String,
    pub context_tokens: u32,
    pub api_key: Option<String>,
    /// Profile name that was resolved (for display)
    pub profile_name: String,
    /// Optional separate model for plan generation (None = use `model` for both)
    pub planner_model: Option<String>,
    /// MCP servers configured for this profile
    pub mcp_servers: Vec<McpServerConfig>,
    /// Optional cost per 1M input tokens in USD (for plan estimates)
    pub cost_per_mtok_input: Option<f64>,
    /// Hook commands from profile config (may be empty — auto-detect handles that)
    pub hooks: crate::hooks::HookConfig,
    /// When true, all hooks are suppressed including auto-detected ones
    pub hooks_disabled: bool,
    /// Auto-commit all changes after each successful task
    pub auto_commit: bool,
    /// Prefix for auto-commit messages
    pub auto_commit_prefix: String,
    /// Enable git integration (checkpoints, status injection, post-task diffs)
    pub git_context: bool,
}

impl ResolvedConfig {
    /// Merge config file profile with CLI overrides.
    /// Priority: CLI args > env vars (handled by clap) > config file profile > built-in defaults
    pub fn resolve(
        file: &ConfigFile,
        profile_override: Option<&str>,
        endpoint_override: Option<&str>,
        model_override: Option<&str>,
        api_key_override: Option<&str>,
    ) -> Self {
        let profile_name = profile_override
            .unwrap_or(&file.default_profile)
            .to_string();

        let base = file
            .resolve_profile(profile_override)
            .cloned()
            .unwrap_or_default();

        Self {
            endpoint: endpoint_override
                .map(str::to_string)
                .unwrap_or(base.endpoint),
            model: model_override
                .map(str::to_string)
                .unwrap_or(base.model),
            context_tokens: base.context_tokens,
            api_key: api_key_override
                .map(str::to_string)
                .or(base.api_key),
            profile_name,
            planner_model: base.planner_model,
            mcp_servers: base.mcp_servers,
            cost_per_mtok_input: base.cost_per_mtok_input,
            hooks: base.hooks,
            hooks_disabled: base.hooks_disabled,
            auto_commit: base.auto_commit,
            auto_commit_prefix: base.auto_commit_prefix,
            git_context: base.git_context,
        }
    }
}

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn config_path() -> PathBuf {
    dirs_config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("parecode")
        .join("config.toml")
}

fn dirs_config_dir() -> Option<PathBuf> {
    // XDG_CONFIG_HOME or ~/.config on Linux/macOS, %APPDATA% on Windows
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
}

// ── Default config template written on first run ──────────────────────────────

const DEFAULT_CONFIG_TOML: &str = r#"# PareCode configuration
# Run `parecode --init` to regenerate this file.

default_profile = "local"

# ── Local Ollama (default) ────────────────────────────────────────────────────
[profiles.local]
endpoint      = "http://localhost:11434"
model         = "qwen3:14b"
context_tokens = 32768
# api_key is not needed for Ollama

# ── Another local model example ───────────────────────────────────────────────
# [profiles.small]
# endpoint      = "http://localhost:11434"
# model         = "qwen3:8b"
# context_tokens = 32768

# ── Anthropic Claude ─────────────────────────────────────────────────────────
# [profiles.claude]
# endpoint             = "https://api.anthropic.com/v1"
# model                = "claude-sonnet-4-6"
# context_tokens       = 200000
# api_key              = "sk-ant-..."
# cost_per_mtok_input  = 3.0   # USD per 1M input tokens — enables cost estimates in /plan

# ── Anthropic Claude — Opus planner + Haiku executor ─────────────────────────
# Uses Opus for planning (high reasoning, low token count) and Haiku for
# executing each step (fast, cheap). Best cost/quality ratio for large tasks.
# [profiles.claude-split]
# endpoint       = "https://api.anthropic.com/v1"
# model          = "claude-haiku-4-5-20251001"
# planner_model  = "claude-opus-4-6"
# context_tokens = 200000
# api_key        = "sk-ant-..."

# ── OpenAI ───────────────────────────────────────────────────────────────────
# [profiles.openai]
# endpoint       = "https://api.openai.com/v1"
# model          = "gpt-4o"
# context_tokens = 128000
# api_key        = "sk-..."

# ── OpenRouter ────────────────────────────────────────────────────────────────
# [profiles.openrouter]
# endpoint       = "https://openrouter.ai/api/v1"
# model          = "qwen/qwen-2.5-coder-32b-instruct"
# context_tokens = 32768
# api_key        = "sk-or-..."

# ── Git integration (optional, per-profile) ──────────────────────────────────
# git_context = true           # inject git status into system prompt; enables checkpoints/diffs
# auto_commit = false          # auto-commit all changes after each successful task
# auto_commit_prefix = "parecode: "

# ── MCP servers (optional, per-profile) ──────────────────────────────────────
# Add MCP servers to any profile to give the model extra tools.
# Tools appear as "<server_name>.<tool_name>" (e.g. "brave.brave_web_search").
#
# Example: Brave Search (requires BRAVE_API_KEY — free at https://brave.com/search/api/)
# [[profiles.local.mcp_servers]]
# name    = "brave"
# command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]
# [profiles.local.mcp_servers.env]
# BRAVE_API_KEY = "BSA..."
#
# Example: Filesystem access (read/write beyond cwd)
# [[profiles.local.mcp_servers]]
# name    = "fs"
# command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
#
# Example: Fetch (HTTP fetch + HTML→text, no API key needed)
# [[profiles.local.mcp_servers]]
# name    = "fetch"
# command = ["uvx", "mcp-server-fetch"]
"#;
