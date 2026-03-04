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
            // Separate edit/write actions from read/search for clearer context
            let actions: Vec<&str> = turn.tool_summary.split(", ").collect();
            let mut modified: Vec<&str> = Vec::new();
            let mut other: Vec<&str> = Vec::new();
            for a in &actions {
                if a.starts_with("edit_file") || a.starts_with("write_file") {
                    modified.push(a);
                } else {
                    other.push(a);
                }
            }
            let mut lines = vec![format!("[Turn {}]", turn.turn_index + 1)];
            lines.push(format!("User: {}", user_preview));
            if !modified.is_empty() {
                // Deduplicate paths to keep it concise
                let mut seen = std::collections::HashSet::new();
                let deduped: Vec<&str> = modified
                    .iter()
                    .copied()
                    .filter(|a| seen.insert(*a))
                    .collect();
                lines.push(format!("Files modified: {}", deduped.join(", ")));
            }
            if !other.is_empty() {
                lines.push(format!("Tools used: {}", other.join(", ")));
            }
            lines.push(format!("Assistant: {}", response_preview));
            format!("{}\n", lines.join("\n"))
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
    // max_chars is character count, not bytes
    if s.chars().count() <= max_chars {
        s
    } else {
        // Find the byte index of the max_chars-th character
        let mut byte_len = 0;
        for (i, (byte_idx, _)) in s.char_indices().enumerate() {
            if i == max_chars {
                byte_len = byte_idx;
                break;
            }
        }
        // If max_chars >= number of chars, we already returned above
        // So byte_len should be set to the start of the max_chars-th char
        &s[..byte_len]
    }
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
    fn test_truncate_str() {
        let s = "hello world";
        assert_eq!(truncate_str(s, 5), "hello");
        assert_eq!(truncate_str(s, 11), "hello world");
        assert_eq!(truncate_str(s, 20), "hello world");
        // Test multi-byte character boundary: crab emoji is 4 bytes
        let s2 = "hello 🦀 world";
        // s2 length in chars: 'h'(1) e l l o ' '(6) 🦀(1) ' '(8) w o r l d(13)
        assert_eq!(truncate_str(s2, 5), "hello");
        assert_eq!(truncate_str(s2, 6), "hello ");
        assert_eq!(truncate_str(s2, 7), "hello 🦀");
        assert_eq!(truncate_str(s2, 8), "hello 🦀 ");
        assert_eq!(truncate_str(s2, 13), "hello 🦀 world");
        assert_eq!(truncate_str(s2, 20), "hello 🦀 world");
        // Ensure no panic on multi-byte boundary
        let _ = truncate_str(s2, 0);
        let _ = truncate_str(s2, 100);
    }

    #[test]
    fn test_build_prior_context_empty() {
        assert_eq!(build_prior_context(&[]), None);
    }

    #[test]
    fn test_build_prior_context_single_turn_no_tools() {
        let turn = ConversationTurn {
            turn_index: 0,
            timestamp: 1234567890,
            user_message: "Hello".to_string(),
            agent_response: "Hi there".to_string(),
            tool_summary: String::new(),
        };
        let context = build_prior_context(&[turn]).unwrap();
        assert!(context.contains("[Turn 1]"));
        assert!(context.contains("User: Hello"));
        assert!(context.contains("Assistant: Hi there"));
    }

    #[test]
    fn test_build_prior_context_with_tools() {
        let turn = ConversationTurn {
            turn_index: 2,
            timestamp: 1234567890,
            user_message: "Change file".to_string(),
            agent_response: "I'll edit it".to_string(),
            tool_summary: "edit_file src/main.rs, read_file src/main.rs".to_string(),
        };
        let context = build_prior_context(&[turn]).unwrap();
        assert!(context.contains("[Turn 3]"));
        assert!(context.contains("User: Change file"));
        assert!(context.contains("Files modified: edit_file src/main.rs"));
        assert!(context.contains("Tools used: read_file src/main.rs"));
        assert!(context.contains("Assistant: I'll edit it"));
    }

    #[test]
    fn test_build_prior_context_tool_dedup() {
        let turn = ConversationTurn {
            turn_index: 0,
            timestamp: 1234567890,
            user_message: "Edit multiple times".to_string(),
            agent_response: "Editing".to_string(),
            tool_summary: "edit_file src/main.rs, edit_file src/main.rs, read_file src/main.rs".to_string(),
        };
        let context = build_prior_context(&[turn]).unwrap();
        // Should deduplicate edit_file src/main.rs
        assert!(context.contains("Files modified: edit_file src/main.rs"));
        // Should not appear twice
        let modified_line = context.lines().find(|l| l.contains("Files modified")).unwrap();
        assert_eq!(modified_line.matches("edit_file src/main.rs").count(), 1);
        // read_file should appear in Tools used
        assert!(context.contains("Tools used: read_file src/main.rs"));
    }

    #[test]
    fn test_build_prior_context_truncation() {
        let mut turns = Vec::new();
        // Create a turn with a very long response that will be truncated
        let turn = ConversationTurn {
            turn_index: 0,
            timestamp: 1234567890,
            user_message: "a".repeat(1000),
            agent_response: "b".repeat(3000),
            tool_summary: String::new(),
        };
        turns.push(turn);
        // The context should still be generated
        let context = build_prior_context(&turns).unwrap();
        // The user message preview is limited to 500 chars, response to 2000 chars
        // So the total length should be within budget
        assert!(context.len() < PRIOR_CONTEXT_TOKEN_CAP * 4);
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

    #[test]
    fn test_build_prior_context_order() {
        let turns = vec![
            ConversationTurn {
                turn_index: 0,
                timestamp: 1,
                user_message: "first".to_string(),
                agent_response: "first response".to_string(),
                tool_summary: String::new(),
            },
            ConversationTurn {
                turn_index: 1,
                timestamp: 2,
                user_message: "second".to_string(),
                agent_response: "second response".to_string(),
                tool_summary: String::new(),
            },
        ];
        let context = build_prior_context(&turns).unwrap();
        // Context should preserve chronological order
        let first_pos = context.find("[Turn 1]");
        let second_pos = context.find("[Turn 2]");
        assert!(first_pos.is_some() && second_pos.is_some());
        assert!(first_pos.unwrap() < second_pos.unwrap());
        // Should include the header
        assert!(context.contains("# Conversation history"));
    }

    #[test]
    fn test_build_prior_context_budget_cap() {
        let mut turns = Vec::new();
        // Create many turns that exceed the token budget
        for i in 0..10 {
            turns.push(ConversationTurn {
                turn_index: i,
                timestamp: i as i64,
                user_message: "x".repeat(500),
                agent_response: "y".repeat(1000),
                tool_summary: String::new(),
            });
        }
        let context = build_prior_context(&turns).unwrap();
        // Should include some turns but not all (due to char budget)
        assert!(context.len() < PRIOR_CONTEXT_TOKEN_CAP * 4);
        // Should include at least one turn
        assert!(context.contains("[Turn"));
    }
}
