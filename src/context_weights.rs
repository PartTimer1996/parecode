/// PIE Phase 3 — Context Weights.
///
/// Learns which files are consistently useful vs consistently wasted.
/// After each task:
///   - files_modified  → weight += 0.1  (were definitely useful)
///   - files_in_context but NOT modified → weight -= 0.05  (probably wasted)
///
/// Weights are clamped to [0.2, 3.0]. Default = 1.0.
/// Persisted to `.parecode/context_weights.json`.
///
/// Used by to_context_package() to surface high-weight clusters first.
use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

const WEIGHTS_PATH: &str = ".parecode/context_weights.json";
const WEIGHT_MIN: f32 = 0.2;
const WEIGHT_MAX: f32 = 3.0;
const WEIGHT_DEFAULT: f32 = 1.0;
const WEIGHT_USEFUL: f32 = 0.1;
const WEIGHT_WASTED: f32 = 0.05;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ContextWeights {
    /// file path → weight multiplier (default 1.0, clamped [0.2, 3.0])
    pub file_weights: HashMap<String, f32>,
}

impl ContextWeights {
    /// Load from `.parecode/context_weights.json`, or return default.
    pub fn load() -> Self {
        Self::load_from(Path::new(WEIGHTS_PATH))
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to `.parecode/context_weights.json`.
    pub fn save(&self) {
        let _ = self.save_to(Path::new(WEIGHTS_PATH));
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let json = serde_json::to_string(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Get weight for a file (defaults to 1.0 if not tracked).
    pub fn _get(&self, file: &str) -> f32 {
        self.file_weights.get(file).copied().unwrap_or(WEIGHT_DEFAULT)
    }

    /// Adjust weights after a completed task.
    ///
    /// `files_modified` = files the agent wrote/patched (useful)
    /// `files_in_context` = all files loaded into context
    pub fn adjust(&mut self, files_modified: &[String], files_in_context: &[String]) {
        // Files that were modified — definitely useful
        for file in files_modified {
            let w = self.file_weights.entry(file.clone()).or_insert(WEIGHT_DEFAULT);
            *w = (*w + WEIGHT_USEFUL).min(WEIGHT_MAX);
        }

        // Files in context but NOT modified — probably wasted
        for file in files_in_context {
            if !files_modified.contains(file) {
                let w = self.file_weights.entry(file.clone()).or_insert(WEIGHT_DEFAULT);
                *w = (*w - WEIGHT_WASTED).max(WEIGHT_MIN);
            }
        }
    }

    /// Mean weight of the given files. Returns 1.0 for empty input.
    pub fn _mean_weight(&self, files: &[String]) -> f32 {
        if files.is_empty() {
            return WEIGHT_DEFAULT;
        }
        let sum: f32 = files.iter().map(|f| self._get(f)).sum();
        sum / files.len() as f32
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Test 1 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_adjust_increases_modified_files() {
        let mut w = ContextWeights::default();
        w.adjust(&["src/plan.rs".to_string()], &["src/plan.rs".to_string(), "src/config.rs".to_string()]);

        // Modified file should increase
        assert!(w._get("src/plan.rs") > WEIGHT_DEFAULT, "modified file should be > 1.0");
        // Context-only file should decrease
        assert!(w._get("src/config.rs") < WEIGHT_DEFAULT, "wasted file should be < 1.0");
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_weight_clamp() {
        let mut w = ContextWeights::default();
        let file = "src/plan.rs".to_string();

        // Drive weight to ceiling
        for _ in 0..50 {
            w.adjust(&[file.clone()], &[file.clone()]);
        }
        assert!(w._get(&file) <= WEIGHT_MAX, "weight should not exceed max");

        // Drive weight to floor with wasted-only adjustments
        let mut w2 = ContextWeights::default();
        for _ in 0..50 {
            w2.adjust(&[], &[file.clone()]);
        }
        assert!(w2._get(&file) >= WEIGHT_MIN, "weight should not go below min");
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_save_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("weights.json");

        let mut w = ContextWeights::default();
        w.adjust(&["src/plan.rs".to_string()], &["src/plan.rs".to_string()]);
        w.save_to(&path).unwrap();

        let loaded = ContextWeights::load_from(&path);
        let delta = (loaded._get("src/plan.rs") - w._get("src/plan.rs")).abs();
        assert!(delta < 0.001, "round-trip weight should match");
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_mean_weight_empty() {
        let w = ContextWeights::default();
        assert!((w._mean_weight(&[]) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_mean_weight_mixed() {
        let mut w = ContextWeights::default();
        // Bump one file up, leave another at default
        for _ in 0..5 {
            w.adjust(&["src/plan.rs".to_string()], &["src/plan.rs".to_string()]);
        }
        let files = vec!["src/plan.rs".to_string(), "src/config.rs".to_string()];
        let mean = w._mean_weight(&files);
        // mean should be > default (one file is high, one is default 1.0)
        assert!(mean > WEIGHT_DEFAULT, "mean should be > 1.0 when one file is elevated");
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.json");
        let w = ContextWeights::load_from(&path);
        assert!(w.file_weights.is_empty());
        assert!((w._get("anything") - WEIGHT_DEFAULT).abs() < 0.001);
    }
}
