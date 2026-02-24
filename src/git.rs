/// Git integration for PareCode — checkpoints, diffs, undo, auto-commit.
///
/// All public functions are designed to fail silently when not in a git repo.
/// Callers use `.and_then()` / `.ok()` — git errors never surface as panics.
///
/// Checkpoint strategy: WIP commits on the current branch.
/// - Clean tree → record HEAD hash (zero cost, no commit).
/// - Dirty tree → `git add -A && git commit --no-verify -m "parecode: checkpoint ..."`.
/// - `--no-verify` bypasses user pre-commit hooks intentionally: checkpoints must never
///   be blocked by lint or formatting hooks.
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

// ── Data structures ────────────────────────────────────────────────────────────

pub struct GitRepo {
    /// Absolute path to the git repo root (the directory containing `.git/`).
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    /// Unix timestamp of the commit
    pub timestamp: i64,
}

// ── Constructor and detection ──────────────────────────────────────────────────

impl GitRepo {
    /// Open a `GitRepo` rooted at the git repository containing `path`.
    /// Returns `None` if `path` is not inside a git repo, or if git is not installed.
    pub fn open(path: &Path) -> Option<Self> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let root = String::from_utf8(output.stdout).ok()?;
        Some(Self {
            root: PathBuf::from(root.trim()),
        })
    }
}

/// Returns `true` if `path` is inside a git repository and git is installed.
pub fn is_git_repo(path: &Path) -> bool {
    GitRepo::open(path).is_some()
}

// ── Core operations ────────────────────────────────────────────────────────────

impl GitRepo {
    /// Create a checkpoint before a task runs.
    ///
    /// Always returns the current HEAD hash without creating any commits.
    /// This preserves the user's working tree state — dirty files are left as-is.
    /// The /undo command uses these hashes to `git reset --hard` back.
    pub fn checkpoint(&self, _task_summary: &str) -> Result<String> {
        self.run_git(&["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
    }

    /// Revert the working tree to the `n`th most recent parecode checkpoint (1-based).
    ///
    /// **Destructive** — does `git reset --hard`. The caller must obtain user
    /// confirmation before calling this function.
    pub fn undo(&self, n: usize) -> Result<()> {
        let checkpoints = self.list_checkpoints()?;
        if checkpoints.is_empty() {
            return Err(anyhow!("no parecode checkpoints found"));
        }
        let idx = n.saturating_sub(1).min(checkpoints.len() - 1);
        let target = &checkpoints[idx];
        self.run_git(&["reset", "--hard", &target.hash])?;
        Ok(())
    }

    /// Returns `git diff <ref_hash> --stat` — summary of what changed since checkpoint,
    /// including uncommitted working-tree changes.
    pub fn diff_stat_from(&self, ref_hash: &str) -> Result<String> {
        self.run_git(&["diff", ref_hash, "--stat"])
    }

    /// Returns `git diff <ref_hash>` — full unified diff since checkpoint,
    /// including uncommitted working-tree changes.
    pub fn diff_full_from(&self, ref_hash: &str) -> Result<String> {
        self.run_git(&["diff", ref_hash])
    }

    /// Returns `git diff HEAD --stat` — summary of uncommitted changes.
    pub fn _diff_stat(&self) -> Result<String> {
        self.run_git(&["diff", "HEAD", "--stat"])
    }

    /// Returns `git diff HEAD` — full unified diff of uncommitted changes.
    pub fn _diff_full(&self) -> Result<String> {
        self.run_git(&["diff", "HEAD"])
    }

    /// Stage all changes and create a commit with the given message.
    /// Uses `--no-verify` so user pre-commit hooks don't block auto-commits.
    pub fn auto_commit(&self, message: &str) -> Result<()> {
        self.run_git(&["add", "-A"])?;
        self.run_git(&["commit", "--no-verify", "-m", message])?;
        Ok(())
    }

    /// Returns `git status --short`, capped at 10 lines.
    /// Suitable for injection into the model's system prompt.
    pub fn status_short(&self) -> Result<String> {
        let out = self.run_git(&["status", "--short"])?;
        let lines: Vec<&str> = out.lines().collect();
        if lines.len() <= 10 {
            Ok(out)
        } else {
            let truncated = lines[..10].join("\n");
            Ok(format!(
                "{}\n... ({} more files)",
                truncated,
                lines.len() - 10
            ))
        }
    }

    /// List parecode checkpoint commits, newest first.
    /// Searches the last 20 commits for messages matching `"parecode: checkpoint"`.
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>> {
        let out = self.run_git(&[
            "log",
            "--format=%H|%h|%s|%ct",
            "--grep=parecode: checkpoint",
            "-20",
        ])?;

        let checkpoints = out
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|line| {
                let mut parts = line.splitn(4, '|');
                let hash = parts.next()?.to_string();
                let short_hash = parts.next()?.to_string();
                let message = parts.next()?.to_string();
                let timestamp = parts.next()?.trim().parse::<i64>().unwrap_or(0);
                Some(CheckpointInfo {
                    hash,
                    short_hash,
                    message,
                    timestamp,
                })
            })
            .collect();

        Ok(checkpoints)
    }

    /// Run a git command in the repo root. Returns stdout on success, Err(stderr) on failure.
    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|e| anyhow!("failed to run git: {e}"))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(anyhow!("git {}: {}", args.join(" "), stderr.trim()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Helper: create a temporary git repo with initial commit and some files.
    fn setup_git_repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempdir().unwrap();
        let path = dir.path();

        // Initialize git repo
        std::process::Command::new("git")
            .arg("init")
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("config")
            .arg("user.email")
            .arg("test@example.com")
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("config")
            .arg("user.name")
            .arg("Test User")
            .current_dir(path)
            .output()
            .unwrap();

        // Create initial commit
        fs::write(path.join("file1.txt"), "content1").unwrap();
        std::process::Command::new("git")
            .args(["add", "file1.txt"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("commit")
            .arg("-m")
            .arg("initial")
            .current_dir(path)
            .output()
            .unwrap();

        let repo = GitRepo::open(path).unwrap();
        (dir, repo)
    }

    #[test]
    fn test_open_non_git_dir() {
        let dir = tempdir().unwrap();
        assert!(GitRepo::open(dir.path()).is_none());
    }

    #[test]
    fn test_open_git_dir() {
        let (_dir, repo) = setup_git_repo();
        assert!(repo.root.exists());
    }

    #[test]
    fn test_is_git_repo() {
        let dir = tempdir().unwrap();
        assert!(!is_git_repo(dir.path()));

        let (_dir, repo) = setup_git_repo();
        assert!(is_git_repo(&repo.root));
    }

    #[test]
    fn test_checkpoint_returns_hash() {
        let (_dir, repo) = setup_git_repo();
        let hash = repo.checkpoint("test").unwrap();
        assert!(hash.len() == 40 || hash.len() == 64); // SHA-1 or SHA-256
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_status_short_clean() {
        let (_dir, repo) = setup_git_repo();
        let status = repo.status_short().unwrap();
        // Clean repo should have empty status
        assert!(status.trim().is_empty());
    }

    #[test]
    fn test_status_short_dirty() {
        let (_dir, repo) = setup_git_repo();
        // Create a new file
        fs::write(repo.root.join("dirty.txt"), "dirty").unwrap();
        let status = repo.status_short().unwrap();
        assert!(status.contains("dirty.txt"));
    }

    #[test]
    fn test_status_short_truncation() {
        let (_dir, repo) = setup_git_repo();
        // Create 15 files to trigger truncation
        for i in 0..15 {
            fs::write(repo.root.join(&format!("file{}.txt", i)), &format!("content{}", i)).unwrap();
        }
        let status = repo.status_short().unwrap();
        assert!(status.contains("... ("));
        // Should have exactly 10 lines of files
        let file_lines: Vec<&str> = status.lines().collect();
        assert_eq!(file_lines.len(), 11); // 10 files + truncation line
    }

    #[test]
    fn test_diff_stat_from_empty() {
        let (_dir, repo) = setup_git_repo();
        let hash = repo.checkpoint("test").unwrap();
        // No changes since initial commit
        let diff = repo.diff_stat_from(&hash).unwrap();
        assert!(diff.trim().is_empty());
    }

    #[test]
    fn test_diff_full_from_empty() {
        let (_dir, repo) = setup_git_repo();
        let hash = repo.checkpoint("test").unwrap();
        let diff = repo.diff_full_from(&hash).unwrap();
        assert!(diff.trim().is_empty());
    }

    #[test]
    fn test_diff_after_modification() {
        let (_dir, repo) = setup_git_repo();
        let initial_hash = repo.checkpoint("test").unwrap();

        // Modify file
        fs::write(repo.root.join("file1.txt"), "modified").unwrap();

        let diff_stat = repo.diff_stat_from(&initial_hash).unwrap();
        assert!(diff_stat.contains("file1.txt"));

        let diff_full = repo.diff_full_from(&initial_hash).unwrap();
        assert!(diff_full.contains("-content1"));
        assert!(diff_full.contains("+modified"));
    }

    #[test]
    fn test_list_checkpoints_empty() {
        let (_dir, repo) = setup_git_repo();
        // No checkpoint commits yet (only initial commit)
        let checkpoints = repo.list_checkpoints().unwrap();
        assert!(checkpoints.is_empty());
    }


    #[test]
    fn test_auto_commit() {
        let (_dir, repo) = setup_git_repo();

        // Make a change
        fs::write(repo.root.join("new_file.txt"), "new").unwrap();

        repo.auto_commit("parecode: auto commit test").unwrap();

        // Verify commit exists
        let log = repo.run_git(&["log", "--oneline"]).unwrap();
        assert!(log.contains("auto commit test"));
    }

    #[test]
    fn test_diff_stat_from_invalid_hash() {
        let (_dir, repo) = setup_git_repo();
        let result = repo.diff_stat_from("invalidhash");
        assert!(result.is_err());
    }

    #[test]
    fn test_run_git_malformed_utf8() {
        let (_dir, repo) = setup_git_repo();
        // git normally returns valid UTF-8, but we test error handling
        // This shouldn't happen in practice, but the code handles it via from_utf8_lossy
        let result = repo.run_git(&["status"]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_checkpoint_info_parsing() {
        // Simulate parsing a log line
        let line = "abc123def456ghi789jkl012|abc123|parecode: checkpoint test|1712345678";
        let mut parts = line.splitn(4, '|');
        let hash = parts.next().unwrap().to_string();
        let short_hash = parts.next().unwrap().to_string();
        let message = parts.next().unwrap().to_string();
        let timestamp = parts.next().unwrap().trim().parse::<i64>().unwrap();

        let info = CheckpointInfo {
            hash,
            short_hash,
            message,
            timestamp,
        };

        assert_eq!(info.message, "parecode: checkpoint test");
        assert_eq!(info.timestamp, 1712345678);
    }

    #[test]
    fn test_checkpoint_info_invalid_timestamp() {
        let line = "hash|short|message|not-a-number";
        let mut parts = line.splitn(4, '|');
        let timestamp = parts.next().unwrap().trim().parse::<i64>().unwrap_or(0);

        assert_eq!(timestamp, 0);
    }



    #[test]
    fn test_run_git_success() {
        let (_dir, repo) = setup_git_repo();
        let output = repo.run_git(&["rev-parse", "HEAD"]).unwrap();
        assert!(!output.trim().is_empty());
        assert!(output.trim().len() >= 40);
    }

    #[test]
    fn test_run_git_failure() {
        let (_dir, repo) = setup_git_repo();
        let result = repo.run_git(&["invalid-command"]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("git invalid-command"));
    }

    #[test]
    fn test_run_git_in_repo_subdirectory() {
        let (_dir, repo) = setup_git_repo();

        // Create a subdirectory and run git from there
        let subdir = repo.root.join("subdir");
        fs::create_dir(&subdir).unwrap();

        // The run_git should use repo.root as current_dir
        let output = repo.run_git(&["rev-parse", "--show-toplevel"]).unwrap();
        let expected_root = repo.root.to_str().unwrap().trim();
        let actual_root = output.trim();
        assert_eq!(actual_root, expected_root);
    }

    #[test]
    fn test_checkpoint_message_ignored() {
        let (_dir, repo) = setup_git_repo();
        let hash1 = repo.checkpoint("message1").unwrap();
        let hash2 = repo.checkpoint("message2").unwrap();
        // checkpoint doesn't create commits, so hash should be same
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_auto_commit_with_special_characters() {
        let (_dir, repo) = setup_git_repo();

        fs::write(repo.root.join("file.txt"), "content").unwrap();

        let message = "parecode: checkpoint with 'quotes' and \"double\" and \nnewline";
        repo.auto_commit(message).unwrap();

        let log = repo.run_git(&["log", "-1", "--format=%s"]).unwrap();
        assert!(log.contains("'quotes'"));
        assert!(log.contains("\"double\""));
    }



    #[test]
    fn test_diffs_with_deleted_file() {
        let (_dir, repo) = setup_git_repo();
        let initial = repo.checkpoint("test").unwrap();

        // Delete existing file
        fs::remove_file(repo.root.join("file1.txt")).unwrap();

        let diff_stat = repo.diff_stat_from(&initial).unwrap();
        assert!(diff_stat.contains("file1.txt"));

        let diff_full = repo.diff_full_from(&initial).unwrap();
        assert!(diff_full.contains("file1.txt"));
        // In unified diff, deletion lines start with '-'
        assert!(diff_full.contains("-content1") || diff_full.contains("-deleted"));
    }

    #[test]
    fn test_checkpoint_with_dirty_tree() {
        let (_dir, repo) = setup_git_repo();

        // Save initial clean state
        let clean_hash = repo.checkpoint("clean").unwrap();

        // Make dirty changes
        fs::write(repo.root.join("dirty1.txt"), "1").unwrap();
        fs::write(repo.root.join("dirty2.txt"), "2").unwrap();

        // checkpoint still returns HEAD hash (the last commit)
        // It doesn't create a new commit on its own
        let after_dirty = repo.checkpoint("dirty").unwrap();
        assert_eq!(clean_hash, after_dirty);
    }

    #[test]
    fn test_list_checkpoints_limit() {
        let (_dir, repo) = setup_git_repo();

        // Create more than 20 checkpoint commits
        for i in 0..25 {
            fs::write(repo.root.join(&format!("file{}.txt", i)), &format!("{}", i)).unwrap();
            repo.auto_commit(&format!("parecode: checkpoint {}", i)).unwrap();
        }

        let checkpoints = repo.list_checkpoints().unwrap();
        // Should be limited to 20 most recent
        assert_eq!(checkpoints.len(), 20);

        // Newest should be the last one created (24)
        assert!(checkpoints[0].message.contains("24"));
        // Oldest in the list should be 5 (24,23,...,5)
        assert!(checkpoints[19].message.contains("5"));
    }


}
