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
