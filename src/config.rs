use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// ── MCP server config ─────────────────────────────────────────────────────────

/// Configuration for a single MCP server process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
            endpoint: "http://localhost:11434/v1/chat/completions".to_string(),
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

    /// Named hook configurations switchable at runtime via `/hooks <name>`.
    /// Example: [hooks.rust] / [hooks.typescript]
    #[serde(default)]
    pub hooks: HashMap<String, crate::hooks::HookConfig>,

    /// The currently active hook configuration name (persisted across restarts).
    /// None means no hooks are active.
    #[serde(default)]
    pub active_hooks: Option<String>,
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
        let mut cfg: ConfigFile = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file at {}", path.display()))?;

        // `active_hooks` must be a top-level key but may have ended up inside a
        // [section] due to append-based writing. Scan the raw text for it so it
        // works regardless of where it appears in the file.
        if cfg.active_hooks.is_none() {
            for line in raw.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("active_hooks") {
                    let rest = rest.trim_start_matches(|c: char| c == ' ' || c == '=').trim();
                    if rest.starts_with('"') && rest.ends_with('"') && rest.len() > 1 {
                        cfg.active_hooks = Some(rest[1..rest.len()-1].to_string());
                        break;
                    }
                }
            }
        }

        Ok(cfg)
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
    /// Names of available hook configs from config (for `/hooks list` display)
    pub available_hooks: Vec<String>,
    /// The currently active hook config name (from config file, persisted)
    pub active_hooks: Option<String>,
    /// The resolved HookConfig for the active named hook (empty if none active)
    pub active_hook_config: crate::hooks::HookConfig,
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

        let mut hook_names: Vec<String> = file.hooks.keys().cloned().collect();
        hook_names.sort();

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
            active_hook_config: file.active_hooks.as_deref()
                .and_then(|name| file.hooks.get(name))
                .cloned()
                .unwrap_or_default(),
            available_hooks: hook_names,
            active_hooks: file.active_hooks.clone(),
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
endpoint      = "http://localhost:11434/v1/chat/completions"
model         = "qwen3:14b"
context_tokens = 32768
# api_key is not needed for Ollama

# ── Another local model example ───────────────────────────────────────────────
# [profiles.small]
# endpoint      = "http://localhost:11434/v1/chat/completions"
# model         = "qwen3:8b"
# context_tokens = 32768

# ── Anthropic Claude ─────────────────────────────────────────────────────────
# [profiles.claude]
# endpoint             = "https://api.anthropic.com/v1/chat/completions"
# model                = "claude-sonnet-4-6"
# context_tokens       = 200000
# api_key              = "sk-ant-..."
# cost_per_mtok_input  = 3.0   # USD per 1M input tokens — enables cost estimates in /plan

# ── Anthropic Claude — Opus planner + Haiku executor ─────────────────────────
# Uses Opus for planning (high reasoning, low token count) and Haiku for
# executing each step (fast, cheap). Best cost/quality ratio for large tasks.
# [profiles.claude-split]
# endpoint       = "https://api.anthropic.com/v1/chat/completions"
# model          = "claude-haiku-4-5-20251001"
# planner_model  = "claude-opus-4-6"
# context_tokens = 200000
# api_key        = "sk-ant-..."

# ── OpenAI ───────────────────────────────────────────────────────────────────
# [profiles.openai]
# endpoint       = "https://api.openai.com/v1/chat/completions"
# model          = "gpt-4o"
# context_tokens = 128000
# api_key        = "sk-..."

# ── OpenRouter ────────────────────────────────────────────────────────────────
# [profiles.openrouter]
# endpoint       = "https://openrouter.ai/api/v1/chat/completions"
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default functions ────────────────────────────────────────────────────

    #[test]
    fn test_default_context_tokens() {
        assert_eq!(default_context_tokens(), 32_768);
    }

    #[test]
    fn test_default_auto_commit_prefix() {
        assert_eq!(default_auto_commit_prefix(), "parecode: ".to_string());
    }

    #[test]
    fn test_default_git_context() {
        assert_eq!(default_git_context(), true);
    }

    #[test]
    fn test_default_profile_name() {
        assert_eq!(default_profile_name(), "default".to_string());
    }

    // ── Profile ──────────────────────────────────────────────────────────────

    #[test]
    fn test_profile_default() {
        let profile = Profile::default();
        assert_eq!(profile.endpoint, "http://localhost:11434/v1/chat/completions");
        assert_eq!(profile.model, "qwen3:14b");
        assert_eq!(profile.context_tokens, 32_768);
        assert_eq!(profile.api_key, None);
        assert_eq!(profile.planner_model, None);
        assert_eq!(profile.mcp_servers, Vec::new());
        assert_eq!(profile.cost_per_mtok_input, None);
        assert_eq!(profile.hooks, crate::hooks::HookConfig::default());
        assert_eq!(profile.hooks_disabled, false);
        assert_eq!(profile.auto_commit, false);
        assert_eq!(profile.auto_commit_prefix, "parecode: ".to_string());
        assert_eq!(profile.git_context, true);
    }

    // ── ConfigFile ───────────────────────────────────────────────────────────

    #[test]
    fn test_config_file_resolve_profile() {
        let mut file = ConfigFile::default();
        file.default_profile = "local".to_string();
        let profile = Profile {
            endpoint: "http://example.com".to_string(),
            model: "test".to_string(),
            ..Default::default()
        };
        file.profiles.insert("local".to_string(), profile.clone());
        file.profiles.insert("other".to_string(), Profile::default());

        // Resolve default
        assert_eq!(file.resolve_profile(None), Some(&profile));
        // Resolve explicit existing
        assert_eq!(file.resolve_profile(Some("other")), Some(&Profile::default()));
        // Resolve non-existent
        assert_eq!(file.resolve_profile(Some("missing")), None);
        // Empty config file
        let empty = ConfigFile::default();
        assert_eq!(empty.resolve_profile(None), None);
        assert_eq!(empty.resolve_profile(Some("local")), None);
    }

    // ── ResolvedConfig ───────────────────────────────────────────────────────

    #[test]
    fn test_resolved_config_resolve() {
        let mut file = ConfigFile::default();
        file.default_profile = "local".to_string();
        let profile = Profile {
            endpoint: "http://example.com".to_string(),
            model: "model1".to_string(),
            context_tokens: 1000,
            api_key: Some("key1".to_string()),
            planner_model: Some("planner1".to_string()),
            cost_per_mtok_input: Some(0.5),
            hooks: crate::hooks::HookConfig::default(),
            hooks_disabled: false,
            auto_commit: true,
            auto_commit_prefix: "prefix: ".to_string(),
            git_context: false,
            ..Default::default()
        };
        file.profiles.insert("local".to_string(), profile.clone());
        // Add a hook config
        let mut hooks = HashMap::new();
        hooks.insert("rust".to_string(), crate::hooks::HookConfig::default());
        file.hooks = hooks;
        file.active_hooks = Some("rust".to_string());

        // No overrides
        let resolved = ResolvedConfig::resolve(&file, None, None, None, None);
        assert_eq!(resolved.endpoint, "http://example.com");
        assert_eq!(resolved.model, "model1");
        assert_eq!(resolved.context_tokens, 1000);
        assert_eq!(resolved.api_key, Some("key1".to_string()));
        assert_eq!(resolved.profile_name, "local");
        assert_eq!(resolved.planner_model, Some("planner1".to_string()));
        assert_eq!(resolved.cost_per_mtok_input, Some(0.5));
        assert_eq!(resolved.hooks, crate::hooks::HookConfig::default());
        assert_eq!(resolved.hooks_disabled, false);
        assert_eq!(resolved.auto_commit, true);
        assert_eq!(resolved.auto_commit_prefix, "prefix: ".to_string());
        assert_eq!(resolved.git_context, false);
        assert_eq!(resolved.available_hooks, vec!["rust".to_string()]);
        assert_eq!(resolved.active_hooks, Some("rust".to_string()));
        assert_eq!(resolved.active_hook_config, crate::hooks::HookConfig::default());

        // Override profile
        let other_profile = Profile {
            endpoint: "http://other.com".to_string(),
            model: "model2".to_string(),
            ..Default::default()
        };
        file.profiles.insert("other".to_string(), other_profile);
        let resolved = ResolvedConfig::resolve(&file, Some("other"), None, None, None);
        assert_eq!(resolved.endpoint, "http://other.com");
        assert_eq!(resolved.model, "model2");
        assert_eq!(resolved.profile_name, "other");

        // CLI overrides
        let resolved = ResolvedConfig::resolve(
            &file,
            None,
            Some("http://override.com"),
            Some("override_model"),
            Some("override_key"),
        );
        assert_eq!(resolved.endpoint, "http://override.com");
        assert_eq!(resolved.model, "override_model");
        assert_eq!(resolved.api_key, Some("override_key".to_string()));
        // Other fields from profile unchanged
        assert_eq!(resolved.context_tokens, 1000);
        assert_eq!(resolved.planner_model, Some("planner1".to_string()));
    }

    // ── Serialization ────────────────────────────────────────────────────────

    #[test]
    fn test_profile_serialization_defaults() {
        let toml_str = r#"
            endpoint = "http://localhost:11434/v1/chat/completions"
            model = "qwen3:14b"
        "#;
        let profile: Profile = toml::from_str(toml_str).unwrap();
        assert_eq!(profile.context_tokens, 32_768);
        assert_eq!(profile.api_key, None);
        assert_eq!(profile.planner_model, None);
        assert_eq!(profile.mcp_servers, Vec::new());
        assert_eq!(profile.cost_per_mtok_input, None);
        assert_eq!(profile.hooks, crate::hooks::HookConfig::default());
        assert_eq!(profile.hooks_disabled, false);
        assert_eq!(profile.auto_commit, false);
        assert_eq!(profile.auto_commit_prefix, "parecode: ".to_string());
        assert_eq!(profile.git_context, true);
    }

    #[test]
    fn test_mcp_server_config_serialization() {
        let toml_str = r#"
            name = "brave"
            command = ["npx", "-y", "@modelcontextprotocol/server-brave-search"]
            [env]
            BRAVE_API_KEY = "BSA..."
        "#;
        let config: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name, "brave");
        assert_eq!(config.command, vec!["npx", "-y", "@modelcontextprotocol/server-brave-search"]);
        assert_eq!(config.env.get("BRAVE_API_KEY"), Some(&"BSA...".to_string()));
    }

    // ── Constants ────────────────────────────────────────────────────────────

    #[test]
    fn test_default_config_toml_is_valid() {
        // Ensure the default config template is valid TOML
        toml::from_str::<toml::Value>(DEFAULT_CONFIG_TOML).unwrap();
    }

    // ── Path functions ───────────────────────────────────────────────────────

    #[test]
    fn test_config_path() {
        let path = config_path();
        assert!(path.ends_with("parecode/config.toml"));
    }

    #[test]
    fn test_dirs_config_dir_env() {
        // This is a weak test but ensures the function doesn't panic
        let _ = dirs_config_dir();
    }
}
