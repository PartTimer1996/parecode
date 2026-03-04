/// File read cache with re-read prevention.
///
/// Every file read this session is cached as raw lines. If the model attempts
/// to read the same path again — full or ranged — we serve it from cache:
///   - Full re-read  → cached formatted output (zero FS overhead)
///   - Ranged re-read → slice the cached lines and format on the fly
///   - First ranged read with no prior full read → read full file, cache it,
///     return just the requested range (future reads of any window are free)
///
/// The cache is invalidated on write/edit so re-reads after mutations are
/// always fresh.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::tools::read::format_line;

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[derive(Debug)]
struct Entry {
    /// Raw file lines (no formatting, no hashes) — cheap to store, flexible to serve.
    lines: Vec<String>,
    /// Pre-formatted full output (cached on first full read to avoid re-formatting).
    full_output: Option<String>,
    turn: usize,
    /// File modification time at the point this entry was cached.
    /// Used to detect external changes (e.g. cargo fmt, user edits between tasks).
    mtime: Option<std::time::SystemTime>,
}

#[derive(Default)]
pub struct FileCache {
    entries: HashMap<PathBuf, Entry>,
    current_turn: usize,
}

impl FileCache {
    /// Advance the turn counter (call once per agent loop iteration).
    pub fn next_turn(&mut self) {
        self.current_turn += 1;
    }

    /// Check if a full (unranged) read is cached. Returns a `CacheHit` if so.
    /// Returns `None` if the file has been modified on disk since it was cached.
    pub fn check(&self, path: &str) -> Option<CacheHit> {
        let key = canonical(path);
        let e = self.entries.get(&key)?;
        // Invalidate if the file has been modified externally since caching.
        if mtime_changed(&key, e.mtime) {
            return None;
        }
        e.full_output.as_ref().map(|out| CacheHit {
            content: out.clone(),
            turns_ago: self.current_turn.saturating_sub(e.turn),
            total_lines: e.lines.len(),
        })
    }

    /// Check if a ranged read can be served from cache.
    /// Returns `Some(formatted_excerpt)` if the file is cached and the range is valid.
    /// Returns `None` if the file has been modified on disk since it was cached.
    pub fn check_range(&self, path: &str, start: usize, end: usize) -> Option<RangeHit> {
        let key = canonical(path);
        let e = self.entries.get(&key)?;
        // Invalidate if the file has been modified externally since caching.
        if mtime_changed(&key, e.mtime) {
            return None;
        }
        let total = e.lines.len();
        // Clamp to valid bounds
        let start = start.min(total.saturating_sub(1));
        let end = end.min(total);
        if start >= end {
            return None;
        }
        let mut out = format!(
            "[{path} — lines {}-{} of {} (cached)]\n\n",
            start + 1,
            end,
            total
        );
        for (i, line) in e.lines[start..end].iter().enumerate() {
            out.push_str(&format_line(start + i + 1, line));
        }
        Some(RangeHit {
            content: out,
            turns_ago: self.current_turn.saturating_sub(e.turn),
            range_lines: end - start,
        })
    }

    /// Store a freshly-read full file (raw content string).
    /// Also pre-formats and caches the full output for fast full-read hits.
    pub fn store(&mut self, path: &str, raw_content: String) {
        let key = canonical(path);
        let lines: Vec<String> = raw_content.lines().map(|l| l.to_string()).collect();
        // Pre-format the full output once
        let mut full_out = format!("[{}]\n\n", path);
        for (i, line) in lines.iter().enumerate() {
            full_out.push_str(&format_line(i + 1, line));
        }
        let mtime = file_mtime(&key);
        self.entries.insert(
            key,
            Entry {
                lines,
                full_output: Some(full_out),
                turn: self.current_turn,
                mtime,
            },
        );
    }

    /// Store raw lines from a fresh disk read (used when we read the full file
    /// to prime the cache after a ranged-read miss).
    pub fn store_lines(&mut self, path: &str, lines: Vec<String>) {
        let key = canonical(path);
        // Pre-format full output
        let mut full_out = format!("[{}]\n\n", path);
        for (i, line) in lines.iter().enumerate() {
            full_out.push_str(&format_line(i + 1, line));
        }
        let mtime = file_mtime(&key);
        self.entries.insert(
            key,
            Entry {
                lines,
                full_output: Some(full_out),
                turn: self.current_turn,
                mtime,
            },
        );
    }

    /// Invalidate a path after a write or edit (so the next read is fresh).
    pub fn invalidate(&mut self, path: &str) {
        let key = canonical(path);
        self.entries.remove(&key);
    }

    /// Invalidate any cached paths that appear as substrings in a bash command.
    /// This catches `sed -i`, `patch`, `git checkout`, etc. mutating cached files.
    pub fn invalidate_if_mentioned(&mut self, command: &str) {
        let to_remove: Vec<PathBuf> = self.entries.keys()
            .filter(|path| {
                let path_str = path.to_string_lossy();
                command.contains(path_str.as_ref())
                    || path.file_name()
                        .map(|f| command.contains(&*f.to_string_lossy()))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        for key in to_remove {
            self.entries.remove(&key);
        }
    }
}

pub struct CacheHit {
    pub content: String,
    pub turns_ago: usize,
    /// Total lines in the cached file
    pub total_lines: usize,
}

pub struct RangeHit {
    pub content: String,
    pub turns_ago: usize,
    /// Number of lines in the served range
    pub range_lines: usize,
}

impl CacheHit {
    pub fn into_message(self) -> String {
        // turns_ago == 0: read in this same turn batch — content not yet in history, send it.
        // turns_ago  > 0: content already in message history from a previous turn — stub only.
        if self.turns_ago > 0 {
            let ago = age_str(self.turns_ago);
            return format!(
                "[Already in context — file was read {ago} and is in your message history above. \
                 Use read_file with line_range if you need a specific section, or edit_file to modify it.]"
            );
        }
        format!(
            "[Returning cached version — file was read this turn. \
             Content is shown below.]\n\n{}",
            self.content
        )
    }
}

impl RangeHit {
    pub fn into_message(self) -> String {
        // Same logic: stub if content is already in message history.
        if self.turns_ago > 0 {
            let ago = age_str(self.turns_ago);
            return format!(
                "[Already in context — this range was read {ago} and is in your message history above. \
                 Use read_file with a different line_range if you need another section.]"
            );
        }
        format!(
            "[Returning cached range — file was read this turn. \
             Content is shown below.]\n\n{}",
            self.content
        )
    }
}

fn age_str(turns_ago: usize) -> String {
    match turns_ago {
        0 => "this turn".to_string(),
        1 => "1 turn ago".to_string(),
        n => format!("{n} turns ago"),
    }
}

/// Returns true if the file's current mtime differs from the cached mtime,
/// meaning the file was modified externally since it was cached.
fn mtime_changed(key: &Path, cached_mtime: Option<SystemTime>) -> bool {
    match (file_mtime(key), cached_mtime) {
        (Some(current), Some(cached)) => current != cached,
        _ => false, // if we can't read mtime, assume unchanged
    }
}

fn canonical(path: &str) -> PathBuf {
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_and_full_check() {
        let mut cache = FileCache::default();
        cache.next_turn();
        cache.store("test.txt", "line one\nline two\nline three".to_string());
        let hit = cache.check("test.txt").unwrap();
        assert!(hit.content.contains("line one"));
        assert_eq!(hit.turns_ago, 0);
    }

    #[test]
    fn test_check_range_hit() {
        let mut cache = FileCache::default();
        cache.next_turn();
        cache.store("test.txt", "line one\nline two\nline three\nline four\nline five".to_string());
        // Request lines 2-4 (1-indexed: start=1, end=4 in 0-based terms)
        let hit = cache.check_range("test.txt", 1, 4).unwrap();
        assert!(hit.content.contains("line two"));
        assert!(hit.content.contains("line three"));
        assert!(hit.content.contains("line four"));
        assert!(!hit.content.contains("line one")); // line 1 not in range
        assert!(!hit.content.contains("line five")); // line 5 not in range
    }

    #[test]
    fn test_check_range_miss_on_uncached_file() {
        let cache = FileCache::default();
        assert!(cache.check_range("nonexistent.txt", 0, 10).is_none());
    }

    #[test]
    fn test_ranged_cache_serves_any_window() {
        let mut cache = FileCache::default();
        cache.store("big.rs", (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
        // First window
        let h1 = cache.check_range("big.rs", 0, 30).unwrap();
        assert!(h1.content.contains("line 0"));
        assert!(h1.content.contains("line 29"));
        // Second window — different range, same file
        let h2 = cache.check_range("big.rs", 50, 80).unwrap();
        assert!(h2.content.contains("line 50"));
        assert!(h2.content.contains("line 79"));
        // No disk reads — both served from cache
    }

    #[test]
    fn test_store_lines() {
        let mut cache = FileCache::default();
        let lines: Vec<String> = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        cache.store_lines("f.rs", lines);
        let hit = cache.check("f.rs").unwrap();
        assert!(hit.content.contains("alpha"));
        let range_hit = cache.check_range("f.rs", 0, 2).unwrap();
        assert!(range_hit.content.contains("alpha"));
        assert!(range_hit.content.contains("beta"));
    }

    #[test]
    fn test_invalidate() {
        let mut cache = FileCache::default();
        cache.store("test.txt", "content".to_string());
        cache.invalidate("test.txt");
        assert!(cache.check("test.txt").is_none());
        assert!(cache.check_range("test.txt", 0, 5).is_none());
    }

    #[test]
    fn test_invalidate_if_mentioned() {
        let mut cache = FileCache::default();
        cache.store("test.txt", "content".to_string());
        cache.invalidate_if_mentioned("sed -i test.txt");
        assert!(cache.check("test.txt").is_none());
    }

    #[test]
    fn test_range_clamping() {
        let mut cache = FileCache::default();
        cache.store("small.rs", "a\nb\nc".to_string()); // 3 lines
        // Request beyond end
        let hit = cache.check_range("small.rs", 0, 999).unwrap();
        assert!(hit.content.contains("a"));
        assert!(hit.content.contains("c"));
    }

    #[test]
    fn test_age_str() {
        assert_eq!(age_str(0), "this turn");
        assert_eq!(age_str(1), "1 turn ago");
        assert_eq!(age_str(5), "5 turns ago");
    }

    #[test]
    fn test_range_hit_message_prefix() {
        let msg = RangeHit { content: "x".to_string(), turns_ago: 2, range_lines: 10 }.into_message();
        assert!(msg.contains("cached range"));
        assert!(msg.contains("2 turns ago"));
    }

    #[test]
    fn test_full_hit_message_prefix() {
        let msg = CacheHit { content: "x".to_string(), turns_ago: 1, total_lines: 50 }.into_message();
        assert!(msg.contains("cached version"));
        assert!(msg.contains("1 turn ago"));
    }
}
