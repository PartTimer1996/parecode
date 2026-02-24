/// Telemetry — global usage stats, persisted to `~/.local/share/parecode/telemetry.jsonl`.
///
/// Stats are:
/// - Accumulated live in AppState during a TUI session
/// - Flushed to disk after every completed agent run
/// - Displayed in the TUI stats tab (key 3)
///
/// The JSONL format keeps one record per completed task (AgentDone event),
/// enabling aggregation across all sessions and projects.
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

// ── Storage path ──────────────────────────────────────────────────────────────

fn telemetry_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local/share")
        });
    base.join("parecode").join("telemetry.jsonl")
}

// ── Per-task record (one line in telemetry.jsonl) ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub timestamp: i64,
    pub session_id: String,
    /// Project directory basename (e.g. "parecode", "my-app")
    #[serde(default)]
    pub cwd: String,
    /// First 80 chars of the user message
    pub task_preview: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub tool_calls: usize,
    pub compressed_count: usize,
    pub compression_ratio: f32,
    /// Wall-clock seconds the task took (0 if not recorded)
    #[serde(default)]
    pub duration_secs: u32,
    pub model: String,
    pub profile: String,
}

// ── Live session stats (held in AppState, reset on /new) ─────────────────────

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    /// Number of completed agent runs this session
    pub tasks_completed: usize,
    /// Cumulative input tokens across all runs
    pub total_input_tokens: u32,
    /// Cumulative output tokens across all runs
    pub total_output_tokens: u32,
    /// Cumulative tool calls across all runs
    pub total_tool_calls: usize,
    /// Cumulative compressed tool outputs across all runs
    pub total_compressed: usize,
    /// Number of context budget enforcement events
    pub budget_enforcements: usize,
    /// Peak context % seen this session (0–100)
    pub peak_context_pct: u32,
    /// Per-task records accumulated this session (for display)
    pub records: Vec<TaskRecord>,

    // ── In-flight tracking (current task, before AgentDone) ────────────────
    /// Input tokens accumulated so far in the currently-running task
    pub inflight_input_tokens: u32,
    /// Output tokens accumulated so far in the currently-running task
    pub inflight_output_tokens: u32,
    /// Tool calls executed so far in the currently-running task
    pub inflight_tool_calls: usize,
    /// Timestamp (epoch secs) of last periodic telemetry flush for the current task
    pub last_flush_ts: i64,
}

impl SessionStats {
    /// Record a completed agent run. Returns the TaskRecord for persistence.
    #[allow(clippy::too_many_arguments)]
    pub fn record_task(
        &mut self,
        session_id: &str,
        cwd: &str,
        task_preview: &str,
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: usize,
        compressed_count: usize,
        duration_secs: u32,
        model: &str,
        profile: &str,
    ) -> TaskRecord {
        self.tasks_completed += 1;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_tool_calls += tool_calls;
        self.total_compressed += compressed_count;

        let compression_ratio = if tool_calls > 0 {
            compressed_count as f32 / tool_calls as f32
        } else {
            0.0
        };

        let record = TaskRecord {
            timestamp: Utc::now().timestamp(),
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            task_preview: task_preview.chars().take(80).collect(),
            input_tokens,
            output_tokens,
            tool_calls,
            compressed_count,
            compression_ratio,
            duration_secs,
            model: model.to_string(),
            profile: profile.to_string(),
        };
        self.records.push(record.clone());
        record
    }

    pub fn update_peak_context(&mut self, pct: u32) {
        if pct > self.peak_context_pct {
            self.peak_context_pct = pct;
        }
    }

    pub fn record_budget_enforcement(&mut self) {
        self.budget_enforcements += 1;
    }

    pub fn total_tokens(&self) -> u32 {
        self.total_input_tokens + self.total_output_tokens
    }

    pub fn avg_tokens_per_task(&self) -> u32 {
        if self.tasks_completed == 0 { return 0; }
        self.total_tokens() / self.tasks_completed as u32
    }

    pub fn compression_ratio(&self) -> f32 {
        if self.total_tool_calls == 0 { return 0.0; }
        self.total_compressed as f32 / self.total_tool_calls as f32
    }

    /// Update in-flight counters from a TokenStats event.
    /// Called on every API response so stats are always current.
    pub fn update_inflight(&mut self, total_input: u32, total_output: u32, tool_calls: usize) {
        self.inflight_input_tokens = total_input;
        self.inflight_output_tokens = total_output;
        self.inflight_tool_calls = tool_calls;
    }

    /// Reset in-flight counters (called when a task completes or errors out).
    pub fn clear_inflight(&mut self) {
        self.inflight_input_tokens = 0;
        self.inflight_output_tokens = 0;
        self.inflight_tool_calls = 0;
        self.last_flush_ts = 0;
    }

    /// Check whether enough time has elapsed to justify a periodic flush.
    /// Returns true (and resets the timer) if ≥ `interval_secs` since last flush.
    pub fn should_flush(&mut self, interval_secs: i64) -> bool {
        let now = Utc::now().timestamp();
        if self.last_flush_ts == 0 || now - self.last_flush_ts >= interval_secs {
            self.last_flush_ts = now;
            true
        } else {
            false
        }
    }

    /// Total tokens including the currently in-flight task.
    #[allow(dead_code)]
    pub fn live_total_tokens(&self) -> u32 {
        self.total_input_tokens + self.total_output_tokens
            + self.inflight_input_tokens + self.inflight_output_tokens
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Append a task record to the global telemetry file.
/// Silently ignores write errors — telemetry must never crash the agent.
pub fn append_record(record: &TaskRecord) {
    let _ = try_append_record(record);
}

fn try_append_record(record: &TaskRecord) -> Result<()> {
    let path = telemetry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Load all records from the global telemetry file, oldest-first.
pub fn load_all() -> Vec<TaskRecord> {
    let path = telemetry_path();
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}


/// Delete all telemetry data. Returns Ok(()) if file didn't exist.
pub fn clear_all() -> Result<()> {
    let path = telemetry_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ── Aggregate helpers ─────────────────────────────────────────────────────────

pub struct Aggregate {
    pub tasks: usize,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub tool_calls: usize,
    pub compressed: usize,
    pub duration_secs: u32,
}

impl Aggregate {
    pub fn from_records(records: &[TaskRecord]) -> Self {
        Self {
            tasks: records.len(),
            input_tokens: records.iter().map(|r| r.input_tokens).sum(),
            output_tokens: records.iter().map(|r| r.output_tokens).sum(),
            tool_calls: records.iter().map(|r| r.tool_calls).sum(),
            compressed: records.iter().map(|r| r.compressed_count).sum(),
            duration_secs: records.iter().map(|r| r.duration_secs).sum(),
        }
    }

    pub fn total_tokens(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }

    pub fn compression_ratio(&self) -> f32 {
        if self.tool_calls == 0 { return 0.0; }
        self.compressed as f32 / self.tool_calls as f32
    }

    pub fn _avg_tokens_per_task(&self) -> u32 {
        if self.tasks == 0 { return 0; }
        self.total_tokens() / self.tasks as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_record_serialization() {
        let record = TaskRecord {
            timestamp: 1625145600,
            session_id: "test_session".to_string(),
            cwd: "my-project".to_string(),
            task_preview: "test task".to_string(),
            input_tokens: 100,
            output_tokens: 200,
            tool_calls: 5,
            compressed_count: 2,
            compression_ratio: 0.4,
            duration_secs: 12,
            model: "test_model".to_string(),
            profile: "test_profile".to_string(),
        };

        let json = serde_json::to_string(&record).unwrap();
        let deserialized: TaskRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(record.timestamp, deserialized.timestamp);
        assert_eq!(record.session_id, deserialized.session_id);
        assert_eq!(record.task_preview, deserialized.task_preview);
        assert_eq!(record.input_tokens, deserialized.input_tokens);
        assert_eq!(record.output_tokens, deserialized.output_tokens);
        assert_eq!(record.tool_calls, deserialized.tool_calls);
        assert_eq!(record.compressed_count, deserialized.compressed_count);
        assert_eq!(record.compression_ratio, deserialized.compression_ratio);
        assert_eq!(record.model, deserialized.model);
        assert_eq!(record.profile, deserialized.profile);
    }

    #[test]
    fn test_session_stats_initial_state() {
        let stats = SessionStats::default();
        assert_eq!(stats.tasks_completed, 0);
        assert_eq!(stats.total_input_tokens, 0);
        assert_eq!(stats.total_output_tokens, 0);
        assert_eq!(stats.total_tool_calls, 0);
        assert_eq!(stats.total_compressed, 0);
        assert_eq!(stats.budget_enforcements, 0);
        assert_eq!(stats.peak_context_pct, 0);
        assert!(stats.records.is_empty());
    }
}
