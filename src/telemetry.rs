/// Telemetry — per-session usage stats, persisted to `.forge/telemetry.jsonl`.
///
/// Stats are:
/// - Accumulated live in AppState during a TUI session
/// - Flushed to disk after every completed agent run
/// - Displayed in the TUI stats overlay (Ctrl+T)
///
/// The JSONL format keeps one record per completed task (AgentDone event),
/// enabling later aggregation across sessions.
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

// ── Per-task record (one line in .forge/telemetry.jsonl) ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub timestamp: i64,
    pub session_id: String,
    pub task_preview: String,  // first 80 chars of the user message
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub tool_calls: usize,
    pub compressed_count: usize,
    pub compression_ratio: f32, // compressed_count / tool_calls (0.0 if no tools)
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
}

impl SessionStats {
    /// Record a completed agent run. Returns the TaskRecord for persistence.
    pub fn record_task(
        &mut self,
        session_id: &str,
        task_preview: &str,
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: usize,
        compressed_count: usize,
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
            task_preview: task_preview.chars().take(80).collect(),
            input_tokens,
            output_tokens,
            tool_calls,
            compressed_count,
            compression_ratio,
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

    /// Total tokens this session
    pub fn total_tokens(&self) -> u32 {
        self.total_input_tokens + self.total_output_tokens
    }

    /// Average tokens per task
    pub fn avg_tokens_per_task(&self) -> u32 {
        if self.tasks_completed == 0 {
            return 0;
        }
        self.total_tokens() / self.tasks_completed as u32
    }

    /// Overall compression ratio (what fraction of tool calls got compressed)
    pub fn compression_ratio(&self) -> f32 {
        if self.total_tool_calls == 0 {
            return 0.0;
        }
        self.total_compressed as f32 / self.total_tool_calls as f32
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Append a task record to `.forge/telemetry.jsonl` in the current working directory.
/// Creates the `.forge/` directory if needed. Silently ignores write errors
/// (telemetry must never crash the agent).
pub fn append_record(record: &TaskRecord) {
    let _ = try_append_record(record);
}

fn try_append_record(record: &TaskRecord) -> Result<()> {
    let dir = PathBuf::from(".forge");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("telemetry.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Load recent records from `.forge/telemetry.jsonl` (last N lines).
/// Returns empty vec if file doesn't exist or can't be read.
pub fn load_recent(limit: usize) -> Vec<TaskRecord> {
    let path = PathBuf::from(".forge/telemetry.jsonl");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect::<Vec<TaskRecord>>()
        .into_iter()
        .rev()
        .take(limit)
        .rev()
        .collect()
}

    #[cfg(test)]
    mod tests {
        use super::*;
        
        #[test]
        fn test_task_record_serialization() {
            let record = TaskRecord {
                timestamp: 1625145600,
                session_id: "test_session".to_string(),
                task_preview: "test task".to_string(),
                input_tokens: 100,
                output_tokens: 200,
                tool_calls: 5,
                compressed_count: 2,
                compression_ratio: 0.4,
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
