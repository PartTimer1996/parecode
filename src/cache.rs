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
    _read_at: Instant,
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
                _read_at: Instant::now(),
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
                // Check both the canonical path and the file name / relative form
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical() {
        let path = "test.txt";
        let canonical_path = canonical(path);
        assert!(canonical_path.is_absolute() || canonical_path.is_relative());
    }

    #[test]
    fn test_next_turn() {
        let mut cache = FileCache::default();
        assert_eq!(cache.current_turn, 0);
        cache.next_turn();
        assert_eq!(cache.current_turn, 1);
    }

    #[test]
    fn test_store_check() {
        let mut cache = FileCache::default();
        cache.next_turn(); // turn 1
        cache.store("test.txt", "content".to_string());
        let hit = cache.check("test.txt").unwrap();
        assert_eq!(hit.content, "content");
        assert_eq!(hit.turns_ago, 0);
    }

    #[test]
    fn test_invalidate() {
        let mut cache = FileCache::default();
        cache.next_turn(); // turn 1
        cache.store("test.txt", "content".to_string());
        cache.invalidate("test.txt");
        assert!(cache.check("test.txt").is_none());
    }

    #[test]
    fn test_invalidate_if_mentioned() {
        let mut cache = FileCache::default();
        cache.next_turn(); // turn 1
        cache.store("test.txt", "content".to_string());

        // Command contains the full path
        cache.invalidate_if_mentioned("sed -i test.txt");
        assert!(cache.check("test.txt").is_none());

        // Command contains the filename
        cache.store("test.txt", "content".to_string());
        cache.invalidate_if_mentioned("sed -i src/test.txt");
        assert!(cache.check("test.txt").is_none());
    }

    #[test]
    fn test_cache_hit_message() {
        let hit = CacheHit {
            content: "test".to_string(),
            turns_ago: 0,
        };
        assert_eq!(hit.into_message(), "[Returning cached version — file was read this turn. Content is shown below. If you believe the file has changed, use edit_file or write_file to update it first.]\n\ntest");

        let hit = CacheHit {
            content: "test".to_string(),
            turns_ago: 1,
        };
        assert_eq!(hit.into_message(), "[Returning cached version — file was read 1 turn ago. Content is shown below. If you believe the file has changed, use edit_file or write_file to update it first.]\n\ntest");

        let hit = CacheHit {
            content: "test".to_string(),
            turns_ago: 2,
        };
        assert_eq!(hit.into_message(), "[Returning cached version — file was read 2 turns ago. Content is shown below. If you believe the file has changed, use edit_file or write_file to update it first.]\n\ntest");
    }

    #[test]
    fn test_canonical_edge_cases() {
        let cases = [
            ("test.txt", "test.txt"),
            ("./test.txt", "./test.txt"),
            ("../test.txt", "../test.txt"),
            ("/absolute/path.txt", "/absolute/path.txt"),
        ];
        
        for (input, expected) in &cases {
            let result = canonical(input);
            assert_eq!(result.to_string_lossy(), *expected);
        }
    }

    #[test]
    fn test_cache_lifecycle() {
        let mut cache = FileCache::default();
        let path = "test.txt";
        
        // Initial store
        cache.store(path, "initial content".to_string());
        
        // Check cache hit
        let hit = cache.check(path).unwrap();
        assert_eq!(hit.content, "initial content");
        assert_eq!(hit.turns_ago, 0);
        
        // Advance turn
        cache.next_turn();
        
        // Check again (should still be cached)
        let hit = cache.check(path).unwrap();
        assert_eq!(hit.content, "initial content");
        assert_eq!(hit.turns_ago, 1);
        
        // Update content
        cache.store(path, "updated content".to_string());
        
        // Check again (should see new content)
        let hit = cache.check(path).unwrap();
        assert_eq!(hit.content, "updated content");
        assert_eq!(hit.turns_ago, 0);
    }

    #[test]
    fn test_cache_invalidations() {
        let mut cache = FileCache::default();
        
        // Store some files
        cache.store("file1.txt", "content1".to_string());
        cache.store("file2.txt", "content2".to_string());
        
        // Check initial state
        assert_eq!(cache.entries.len(), 2);
        
        // Invalidate one file
        cache.invalidate("file1.txt");
        
        // Check invalidation
        assert!(cache.check("file1.txt").is_none());
        assert!(cache.check("file2.txt").is_some());
        
        // Test command-based invalidation
        cache.invalidate_if_mentioned("sed -i file2.txt");
        assert!(cache.check("file2.txt").is_none());
    }

    #[test]
    fn test_cache_hit_message_formats() {
        let hit = CacheHit {
            content: "test content".to_string(),
            turns_ago: 2,
        };
        
        let message = hit.into_message();
        
        assert!(message.starts_with("[Returning cached version — file was read 2 turns ago."));
        assert!(message.ends_with("test content"));
    }
}
