/// Session persistence for Forge — Phase 3.
///
/// Each TUI session is stored as a JSONL file in `~/.local/share/forge/sessions/`.
/// One line per completed conversation turn (user message + agent response + tool summary).
///
/// Sessions enable:
/// - In-session memory: prior turns injected as preamble on each new agent run
/// - Cross-session resume: load a previous session's turns back into context
/// - Rollback: set active_turn pointer to rewind context without deleting history
use std::cmp::Reverse;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── ConversationTurn ──────────────────────────────────────────────────────────

/// One completed user↔agent exchange, stored as the memory record for this turn.
/// Kept intentionally lean — full tool outputs are NOT stored here (they balloon
/// quickly and aren't needed for context injection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    /// 0-based index within this session
    pub turn_index: usize,
    /// Unix timestamp (seconds) when the turn completed
    pub timestamp: i64,
    /// Raw user message (without the attached-file preamble)
    pub user_message: String,
    /// Full assistant response text
    pub agent_response: String,
    /// Compact list of tool names called, e.g. "read_file, edit_file"
    pub tool_summary: String,
}

// ── Session ───────────────────────────────────────────────────────────────────

pub struct Session {
    /// "{unix_ts}_{cwd_basename}"
    pub id: String,
    /// Absolute cwd when session was created
    pub cwd: String,
    /// All turns recorded so far (including archived ones beyond active_turn)
    pub turns: Vec<ConversationTurn>,
    /// High-water mark: turns[0..=active_turn] are "live" for context injection.
    /// Turns beyond this are archived (rolled back) but not deleted.
    pub active_turn: usize,
    /// Path to the JSONL file on disk
    pub path: PathBuf,
}

// ── Directory helpers ─────────────────────────────────────────────────────────

pub fn sessions_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local/share")
        })
        .join("forge/sessions")
}

fn cwd_basename(cwd: &str) -> &str {
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
}

// ── Session lifecycle ─────────────────────────────────────────────────────────

/// Create a new empty session and ensure the sessions directory exists.
pub fn open_session(cwd: &str) -> Result<Session> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let ts = chrono::Utc::now().timestamp();
    let basename = cwd_basename(cwd);
    let id = format!("{ts}_{basename}");
    let path = dir.join(format!("{id}.jsonl"));

    Ok(Session {
        id,
        cwd: cwd.to_string(),
        turns: Vec::new(),
        active_turn: 0,
        path,
    })
}

/// Append a single turn to the JSONL file (one line = one turn).
/// Called immediately after `finalize_turn()` so data survives crashes.
pub fn append_turn(path: &Path, turn: &ConversationTurn) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(turn)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Load all turns from an existing session JSONL file.
pub fn load_session_turns(path: &Path) -> Result<Vec<ConversationTurn>> {
    let content = std::fs::read_to_string(path)?;
    let turns = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<ConversationTurn>(l))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(turns)
}

/// List all session files in the sessions directory, sorted newest-first.
/// Returns (session_id, path) pairs.
pub fn list_sessions() -> Result<Vec<(String, PathBuf)>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| {
            e.path().extension().map(|x| x == "jsonl").unwrap_or(false)
        })
        .collect();
    // Sort by filename descending (timestamp prefix makes this newest-first)
    entries.sort_by_key(|e| Reverse(e.file_name()));
    Ok(entries
        .iter()
        .map(|e| {
            let name = e
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".jsonl")
                .to_string();
            (name, e.path())
        })
        .collect())
}

/// Find the most recent session file whose name ends with `_{cwd_basename}`.
/// Returns (session_id, path) if found.
pub fn find_latest_for_cwd(cwd: &str) -> Option<(String, PathBuf)> {
    let suffix = format!("_{}", cwd_basename(cwd));
    list_sessions().ok()?.into_iter().find(|(id, _)| id.ends_with(&suffix))
}

// ── Context injection ─────────────────────────────────────────────────────────

/// Cap for injected prior context, in estimated tokens.
/// 8000 tokens ≈ 32000 chars — uses ~25% of a 32k context window for history.
const PRIOR_CONTEXT_TOKEN_CAP: usize = 8000;

/// Build a prior-context string from completed turns for injection into the
/// next agent run. Includes the most recent turns first (within the token cap).
/// Returns None if there are no turns or the turns slice is empty.
pub fn build_prior_context(turns: &[ConversationTurn]) -> Option<String> {
    if turns.is_empty() {
        return None;
    }

    let char_budget = PRIOR_CONTEXT_TOKEN_CAP * 4;
    let mut used = 0usize;
    let mut parts: Vec<String> = Vec::new();

    // Walk newest-first so the most recent turns survive if we hit the cap
    for turn in turns.iter().rev() {
        let response_preview = truncate_str(&turn.agent_response, 2000);
        let user_preview = truncate_str(&turn.user_message, 500);
        let entry = if turn.tool_summary.is_empty() {
            format!(
                "[Turn {}]\nUser: {}\nAssistant: {}\n",
                turn.turn_index + 1,
                user_preview,
                response_preview,
            )
        } else {
            format!(
                "[Turn {}]\nUser: {}\nTools used: {}\nAssistant: {}\n",
                turn.turn_index + 1,
                user_preview,
                turn.tool_summary,
                response_preview,
            )
        };

        if used + entry.len() > char_budget {
            break;
        }
        used += entry.len();
        parts.push(entry);
    }

    if parts.is_empty() {
        return None;
    }

    // Restore chronological order
    parts.reverse();

    Some(format!(
        "# Conversation history (this session)\nNote: short user replies (e.g. \"yes\", \"ok\", \"go ahead\") are responses to the previous assistant message.\n\n{}\n---\n\n",
        parts.join("\n")
    ))
}

fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        // Walk back to a char boundary
        let mut end = max_chars;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}
