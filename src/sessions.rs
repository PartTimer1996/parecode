/// Session persistence for PareCode — Phase 3.
///
/// Each TUI session is stored as a JSONL file in `~/.local/share/parecode/sessions/`.
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
    pub _cwd: String,
    /// All turns recorded so far (including archived ones beyond active_turn)
    pub _turns: Vec<ConversationTurn>,
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
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("parecode/sessions")
}

fn cwd_basename(cwd: &str) -> &str {
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
}

// ── Session lifecycle ─────────────────────────────────────────────────────────

/// Open an existing session for this CWD, or create a new one if none exists.
pub fn open_session(cwd: &str) -> Result<Session> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;

    // Check for existing session to resume - reuse its path instead of creating new
    if let Some((id, path)) = find_latest_for_cwd(cwd) {
        let loaded = load_session_turns(&path)?;
        let active_turn = loaded.len().saturating_sub(1);
        return Ok(Session {
            id,
            _cwd: cwd.to_string(),
            _turns: loaded,
            active_turn,
            path,
        });
    }

    // No existing session - create a new one
    let ts = chrono::Utc::now().timestamp();
    let basename = cwd_basename(cwd);
    let id = format!("{ts}_{basename}");
    let path = dir.join(format!("{id}.jsonl"));

    // Touch the file so list_sessions() can find it
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);

    Ok(Session {
        id,
        _cwd: cwd.to_string(),
        _turns: Vec::new(),
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
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
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

/// Delete session files beyond the newest `keep` *non-empty* sessions, oldest-first.
/// Empty sessions (zero-byte files with no turns) are always deleted immediately.
/// list_sessions() returns newest-first, so we walk in order and keep the first
/// `keep` non-empty ones, deleting everything else.
pub fn prune_old_sessions(keep: usize) {
    let Ok(sessions) = list_sessions() else {
        return;
    };
    let mut kept = 0usize;
    for (_, path) in sessions {
        let is_empty = std::fs::metadata(&path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        if is_empty {
            let _ = std::fs::remove_file(&path);
        } else {
            kept += 1;
            if kept > keep {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Create a brand-new session for this CWD, ignoring any existing sessions.
/// Used by `/new` to force a fresh start.
pub fn new_session(cwd: &str) -> Result<Session> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let ts = chrono::Utc::now().timestamp();
    let basename = cwd_basename(cwd);
    let id = format!("{ts}_{basename}");
    let path = dir.join(format!("{id}.jsonl"));

    // Touch the file so list_sessions() can find it
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);

    Ok(Session {
        id,
        _cwd: cwd.to_string(),
        _turns: Vec::new(),
        active_turn: 0,
        path,
    })
}

/// Find the most recent session file whose name ends with `_{cwd_basename}`.
/// Returns (session_id, path) if found.
pub fn find_latest_for_cwd(cwd: &str) -> Option<(String, PathBuf)> {
    let suffix = format!("_{}", cwd_basename(cwd));
    list_sessions()
        .ok()?
        .into_iter()
        .find(|(id, _)| id.ends_with(&suffix))
}


// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_cwd_basename() {
        assert_eq!(cwd_basename("/home/user/projects/foo"), "foo");
        assert_eq!(cwd_basename("foo"), "foo");
        assert_eq!(cwd_basename("/"), "unknown");
        assert_eq!(cwd_basename(""), "unknown");
        assert_eq!(cwd_basename("/tmp/"), "tmp");
    }






    #[test]
    fn test_append_and_load_turns() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let turn = ConversationTurn {
            turn_index: 0,
            timestamp: 1234567890,
            user_message: "test".to_string(),
            agent_response: "response".to_string(),
            tool_summary: "read_file".to_string(),
        };
        append_turn(path, &turn)?;
        // Append another turn
        let turn2 = ConversationTurn {
            turn_index: 1,
            timestamp: 1234567891,
            user_message: "test2".to_string(),
            agent_response: "response2".to_string(),
            tool_summary: String::new(),
        };
        append_turn(path, &turn2)?;
        let loaded = load_session_turns(path)?;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].user_message, "test");
        assert_eq!(loaded[1].user_message, "test2");
        Ok(())
    }

    #[test]
    fn test_load_session_turns_empty_file() -> Result<()> {
        let file = NamedTempFile::new()?;
        let path = file.path();
        let loaded = load_session_turns(path)?;
        assert_eq!(loaded.len(), 0);
        Ok(())
    }

    #[test]
    fn test_load_session_turns_invalid_json() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path();
        std::fs::write(path, "not json\n").unwrap();
        let result = load_session_turns(path);
        assert!(result.is_err());
    }

    #[test]
    fn test_sessions_dir_env_var() {
        // Just test that the function returns a path containing "parecode/sessions"
        // We can't safely modify env vars in tests without unsafe, so just verify basic behavior
        let path = sessions_dir();
        assert!(path.to_string_lossy().contains("parecode/sessions"));
    }

}
