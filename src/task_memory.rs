/// PIE Phase 3 — Task Memory.
///
/// Append-only log of completed tasks. Persisted to `.parecode/task_memory.jsonl`
/// (one JSON object per line). Each record captures what the task was, what files
/// were modified, and a one-sentence summary extracted from the final assistant
/// message — zero extra model cost.
///
/// At planning/agent time the top-N most relevant records are retrieved and
/// injected into the context package as "Recent relevant tasks".
use std::path::Path;

use serde::{Deserialize, Serialize};

const TASK_MEMORY_PATH: &str = ".parecode/task_memory.jsonl";
/// How many recent records to scan when looking for relevant tasks
const SCAN_LIMIT: usize = 200;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Unique ID — timestamp-based, sortable
    pub id: String,
    /// Unix timestamp of task completion
    pub timestamp: i64,
    /// First 200 chars of the user's task description
    pub task: String,
    /// "solved" | "partial" | "failed"
    pub outcome: String,
    /// Files the agent actually wrote/patched — proxy for "files that mattered"
    pub files_modified: Vec<String>,
    /// 1-2 sentence summary extracted from the final assistant message.
    /// This is the key field — makes each record self-describing without re-reads.
    pub summary: String,
    /// Total tokens consumed
    pub tokens_used: u32,
    /// Everything loaded into context (for weight adjustment — not injected)
    #[serde(default)]
    pub files_in_context: Vec<String>,
}

impl TaskRecord {
    pub fn new(
        task: &str,
        outcome: &str,
        files_modified: Vec<String>,
        summary: &str,
        tokens_used: u32,
        files_in_context: Vec<String>,
    ) -> Self {
        let ts = chrono::Utc::now().timestamp();
        Self {
            id: format!("{ts}"),
            timestamp: ts,
            task: task.chars().take(200).collect(),
            outcome: outcome.to_string(),
            files_modified,
            summary: summary.chars().take(200).collect(),
            tokens_used,
            files_in_context,
        }
    }

    /// Age in whole days (0 = today)
    pub fn days_ago(&self) -> u32 {
        let now = chrono::Utc::now().timestamp();
        ((now - self.timestamp).max(0) / 86_400) as u32
    }

    /// Human-readable age string: "just now", "2h ago", "3d ago"
    pub fn age_str(&self) -> String {
        let now = chrono::Utc::now().timestamp();
        let secs = (now - self.timestamp).max(0);
        if secs < 120 {
            "just now".to_string()
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86_400 {
            format!("{}h ago", secs / 3600)
        } else {
            let d = secs / 86_400;
            format!("{d}d ago")
        }
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Append a single record to `.parecode/task_memory.jsonl`.
/// Non-fatal — disk errors are silently ignored.
pub fn append_record(record: &TaskRecord) {
    let _ = try_append_record(record);
}

fn try_append_record(record: &TaskRecord) -> anyhow::Result<()> {
    std::fs::create_dir_all(".parecode")?;
    let line = serde_json::to_string(record)?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(TASK_MEMORY_PATH)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Load the most recent `limit` records from disk (newest first).
/// Returns empty vec on any error.
pub fn load_recent(limit: usize) -> Vec<TaskRecord> {
    load_recent_from(Path::new(TASK_MEMORY_PATH), limit)
}

pub fn load_recent_from(path: &Path, limit: usize) -> Vec<TaskRecord> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut records: Vec<TaskRecord> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // Newest first
    records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    records.truncate(limit);
    records
}

// ── Relevance ranking ─────────────────────────────────────────────────────────

/// Find the top-N most relevant past tasks for a given set of candidate files.
///
/// Score = intersection_size × recency_weight
/// where recency_weight = 1.0 / (days_ago + 1)
///
/// `candidate_files` = files likely relevant to the current task
/// (from graph cluster lookup or explicit file refs in the task text)
pub fn find_relevant(candidate_files: &[String], top_n: usize) -> Vec<TaskRecord> {
    find_relevant_from(Path::new(TASK_MEMORY_PATH), candidate_files, top_n)
}

pub fn find_relevant_from(path: &Path, candidate_files: &[String], top_n: usize) -> Vec<TaskRecord> {
    if candidate_files.is_empty() {
        // No candidate files — just return most recent tasks
        return load_recent_from(path, top_n);
    }

    let records = load_recent_from(path, SCAN_LIMIT);
    if records.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, TaskRecord)> = records
        .into_iter()
        .filter_map(|r| {
            let intersection = r
                .files_modified
                .iter()
                .filter(|f| candidate_files.contains(f))
                .count();
            if intersection == 0 {
                return None;
            }
            let recency = 1.0 / (r.days_ago() as f32 + 1.0);
            let score = intersection as f32 * recency;
            Some((score, r))
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(top_n).map(|(_, r)| r).collect()
}

/// Extract a summary from a final assistant message.
/// Takes the first 1-2 sentences, capped at 150 chars.
/// Falls back to a truncated version of the full text.
pub fn extract_summary(response_text: &str) -> String {
    let text = response_text.trim();
    if text.is_empty() {
        return String::new();
    }

    // Try to extract 1-2 clean sentences
    let clean = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-') && !l.starts_with('`'))
        .collect::<Vec<_>>()
        .join(" ");

    // Find sentence boundary around 150 chars
    let cap = 150usize;
    if clean.len() <= cap {
        return clean.chars().take(cap).collect();
    }

    // Try to end at a sentence boundary
    let truncated = &clean[..clean.floor_char_boundary(cap)];
    if let Some(pos) = truncated.rfind(|c| c == '.' || c == '!' || c == '?') {
        let sentence = &truncated[..=pos];
        if sentence.len() > 20 {
            return sentence.to_string();
        }
    }

    format!("{}…", truncated.trim_end())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_records(path: &Path, records: &[TaskRecord]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut out = String::new();
        for r in records {
            out.push_str(&serde_json::to_string(r).unwrap());
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    fn make_record(task: &str, files: &[&str], summary: &str, days_ago: i64) -> TaskRecord {
        let ts = chrono::Utc::now().timestamp() - days_ago * 86_400;
        TaskRecord {
            id: format!("{ts}"),
            timestamp: ts,
            task: task.to_string(),
            outcome: "solved".to_string(),
            files_modified: files.iter().map(|s| s.to_string()).collect(),
            summary: summary.to_string(),
            tokens_used: 1000,
            files_in_context: vec![],
        }
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_append_and_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("task_memory.jsonl");

        let r1 = make_record("fix the login bug", &["src/auth.rs"], "Fixed null check in login handler", 0);
        let r2 = make_record("add dark mode", &["src/tui/mod.rs"], "Added theme toggle to AppState", 1);

        // Write directly
        write_records(&path, &[r1.clone(), r2.clone()]);

        let loaded = load_recent_from(&path, 10);
        assert_eq!(loaded.len(), 2);
        // Newest first (r1 is newer — day 0)
        assert_eq!(loaded[0].task, r1.task);
        assert_eq!(loaded[1].task, r2.task);
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_find_relevant_by_file_intersection() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("task_memory.jsonl");

        let r_plan = make_record("update planner", &["src/plan.rs", "src/narrative.rs"], "Updated generate_plan signature", 1);
        let r_tui = make_record("fix spinner", &["src/tui/mod.rs", "src/tui/render.rs"], "Fixed splash spinner animation", 2);
        let r_unrelated = make_record("update readme", &["README.md"], "Updated documentation", 3);

        write_records(&path, &[r_plan.clone(), r_tui.clone(), r_unrelated.clone()]);

        let candidates = vec!["src/plan.rs".to_string(), "src/narrative.rs".to_string()];
        let relevant = find_relevant_from(&path, &candidates, 3);

        // r_plan should rank first (2 file matches)
        assert!(!relevant.is_empty());
        assert_eq!(relevant[0].task, r_plan.task);
        // r_unrelated should not appear (no file intersection)
        assert!(!relevant.iter().any(|r| r.task == r_unrelated.task));
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_recency_breaks_ties() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("task_memory.jsonl");

        // Both touch the same file, but r_recent is newer
        let r_old = make_record("old task", &["src/plan.rs"], "Old summary", 10);
        let r_recent = make_record("recent task", &["src/plan.rs"], "Recent summary", 1);

        write_records(&path, &[r_old.clone(), r_recent.clone()]);

        let candidates = vec!["src/plan.rs".to_string()];
        let relevant = find_relevant_from(&path, &candidates, 2);

        assert_eq!(relevant.len(), 2);
        // Recent should score higher (same intersection, higher recency weight)
        assert_eq!(relevant[0].task, r_recent.task);
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_extract_summary_short_text() {
        let text = "I updated the multi-turn loop signature.";
        let s = extract_summary(text);
        assert_eq!(s, text);
    }

    #[test]
    fn test_extract_summary_long_text_ends_at_sentence() {
        let text = "I updated the generate_plan function. It now accepts a narrative parameter. \
                    There are many other things in this response that should be truncated away because \
                    they are beyond the 150 character limit we set for summaries.";
        let s = extract_summary(text);
        // Should end at a sentence boundary
        assert!(s.ends_with('.') || s.ends_with('!') || s.ends_with('?') || s.ends_with('…'));
        assert!(s.len() <= 155); // small buffer for the ellipsis
    }

    #[test]
    fn test_extract_summary_skips_markdown() {
        let text = "# Summary\n- Fixed the bug\n- Updated tests\n\nI updated the login handler to fix the null check.";
        let s = extract_summary(text);
        assert!(!s.contains("# Summary"));
        assert!(!s.is_empty());
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_load_recent_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.jsonl");
        let records = load_recent_from(&path, 10);
        assert!(records.is_empty());
    }

    #[test]
    fn test_load_recent_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("task_memory.jsonl");

        let records: Vec<TaskRecord> = (0..10)
            .map(|i| make_record(&format!("task {i}"), &["src/foo.rs"], "summary", i))
            .collect();
        write_records(&path, &records);

        let loaded = load_recent_from(&path, 3);
        assert_eq!(loaded.len(), 3);
    }

    // ── Test 6 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_age_str() {
        let now_rec = make_record("t", &[], "s", 0);
        let old_rec = make_record("t", &[], "s", 5);

        // 0 days ago = "just now" (timestamp is exactly now)
        let age = now_rec.age_str();
        assert!(age == "just now" || age.contains("m ago"), "unexpected: {age}");

        let age_old = old_rec.age_str();
        assert!(age_old.contains("d ago"), "unexpected: {age_old}");
    }
}
