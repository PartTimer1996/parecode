/// File read cache with re-read prevention.
///
/// Every file read this session is cached. If the model attempts to read
/// the same path again we return the cached version instantly with an
/// explanatory note — zero filesystem overhead, zero wasted context tokens.
///
/// The cache is also invalidated on write/edit so re-reads after mutations
/// are always fresh.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug)]
struct Entry {
    content: String,
    turn: usize,
    read_at: Instant,
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

    /// Check if a path is already cached. Returns a cache-hit message if so.
    pub fn check(&self, path: &str) -> Option<CacheHit> {
        let key = canonical(path);
        self.entries.get(&key).map(|e| {
            let turns_ago = self.current_turn.saturating_sub(e.turn);
            CacheHit {
                content: e.content.clone(),
                turns_ago,
            }
        })
    }

    /// Store a freshly-read file.
    pub fn store(&mut self, path: &str, content: String) {
        let key = canonical(path);
        self.entries.insert(
            key,
            Entry {
                content,
                turn: self.current_turn,
                read_at: Instant::now(),
            },
        );
    }

    /// Invalidate a path after a write or edit (so the next read is fresh).
    pub fn invalidate(&mut self, path: &str) {
        let key = canonical(path);
        self.entries.remove(&key);
    }

    /// Number of cached files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub struct CacheHit {
    pub content: String,
    pub turns_ago: usize,
}

impl CacheHit {
    /// Build the message returned to the model instead of a fresh file read.
    pub fn into_message(self) -> String {
        let ago = match self.turns_ago {
            0 => "this turn".to_string(),
            1 => "1 turn ago".to_string(),
            n => format!("{n} turns ago"),
        };
        format!(
            "[Returning cached version — file was read {ago}. \
             Content is shown below. If you believe the file has changed, use edit_file \
             or write_file to update it first.]\n\n{}",
            self.content
        )
    }
}

fn canonical(path: &str) -> PathBuf {
    // Resolve to absolute path if possible; fall back to as-given
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}
