//! Tracks file content state across tool calls so tools can detect
//! concurrent modifications.
//!
//! Typical flow:
//!
//! 1. A read tool reads `/foo/bar.rs` and calls
//!    [`FileStateCache::record_read`] with the content and the file's
//!    on-disk mtime.
//! 2. Later, an edit tool re-reads the file, then calls
//!    [`FileStateCache::is_modified`] to check whether it changed between
//!    the original read and now. If so, it returns
//!    [`crate::ToolError::FileModified`] so the agent can re-read before
//!    editing.
//! 3. After a successful write, the edit tool calls
//!    [`FileStateCache::forget`] so the next read starts a fresh baseline.
//!
//! The cache does no I/O itself — callers supply content and mtime. This
//! keeps the type portable (host or container) and testable without a
//! filesystem.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use tokio::sync::RwLock;

/// Snapshot of a file's state at the moment it was last read.
#[derive(Debug, Clone, Copy)]
pub struct FileState {
    /// Stable hash of the file's content at read time.
    pub content_hash: u64,
    /// Filesystem modification timestamp at read time.
    pub modified_at: SystemTime,
    /// Monotonic timestamp of when the cache observed the file.
    pub read_at: Instant,
}

/// Per-session file state cache.
///
/// Wrap in an [`Arc<RwLock<...>>`](SharedFileStateCache) to share between
/// concurrently-executing tool calls.
#[derive(Debug, Default)]
pub struct FileStateCache {
    states: HashMap<PathBuf, FileState>,
}

/// Shared, async-safe handle to a [`FileStateCache`].
pub type SharedFileStateCache = Arc<RwLock<FileStateCache>>;

impl FileStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was read with the given content and mtime.
    ///
    /// Replaces any previous entry for the same path — the most recent read
    /// wins, which is the right behavior for detecting modifications between
    /// the last observation and a subsequent edit.
    pub fn record_read(
        &mut self,
        path: impl Into<PathBuf>,
        content: &str,
        modified_at: SystemTime,
    ) {
        let state = FileState {
            content_hash: hash_content(content),
            modified_at,
            read_at: Instant::now(),
        };
        self.states.insert(path.into(), state);
    }

    /// Whether the file at `path` has changed since it was last recorded.
    ///
    /// Returns `false` when the path has never been recorded (we can't
    /// claim modification against an unknown baseline).
    ///
    /// Checks mtime first as the fast path; falls back to content hashing
    /// when mtime matches (filesystems can preserve mtime across atomic
    /// rewrites, so content comparison catches those cases).
    pub fn is_modified(
        &self,
        path: &Path,
        current_content: &str,
        current_modified_at: SystemTime,
    ) -> bool {
        let Some(state) = self.states.get(path) else {
            return false;
        };
        if current_modified_at != state.modified_at {
            return true;
        }
        hash_content(current_content) != state.content_hash
    }

    /// Drop cached state for `path`.
    ///
    /// Call after a successful write so the next read establishes a fresh
    /// baseline rather than comparing against pre-write content.
    pub fn forget(&mut self, path: &Path) {
        self.states.remove(path);
    }

    /// Inspect the cached state for `path`, if any.
    pub fn state(&self, path: &Path) -> Option<&FileState> {
        self.states.get(path)
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

fn hash_content(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn unknown_path_is_never_modified() {
        let cache = FileStateCache::new();
        assert!(!cache.is_modified(Path::new("/tmp/foo.rs"), "hello", ts(100)));
    }

    #[test]
    fn same_content_and_mtime_is_not_modified() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "hello", ts(100));
        assert!(!cache.is_modified(Path::new("/tmp/foo.rs"), "hello", ts(100)));
    }

    #[test]
    fn different_mtime_detected_without_hashing() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "hello", ts(100));
        // Content identical, mtime advanced — still reported as modified.
        assert!(cache.is_modified(Path::new("/tmp/foo.rs"), "hello", ts(200)));
    }

    #[test]
    fn same_mtime_different_content_detected_via_hash() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "hello", ts(100));
        // mtime preserved (e.g. atomic rewrite) — hash catches the change.
        assert!(cache.is_modified(Path::new("/tmp/foo.rs"), "goodbye", ts(100)));
    }

    #[test]
    fn subsequent_read_replaces_earlier_snapshot() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "v1", ts(100));
        cache.record_read("/tmp/foo.rs", "v2", ts(200));

        // Baseline is now (v2, ts 200); v1/100 would look modified relative to that.
        assert!(cache.is_modified(Path::new("/tmp/foo.rs"), "v1", ts(100)));
        assert!(!cache.is_modified(Path::new("/tmp/foo.rs"), "v2", ts(200)));
    }

    #[test]
    fn forget_removes_baseline() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "hello", ts(100));
        assert_eq!(cache.len(), 1);

        cache.forget(Path::new("/tmp/foo.rs"));
        assert!(cache.is_empty());
        // After forget, any comparison against unknown returns false.
        assert!(!cache.is_modified(Path::new("/tmp/foo.rs"), "goodbye", ts(200)));
    }

    #[test]
    fn state_exposes_recorded_values() {
        let mut cache = FileStateCache::new();
        cache.record_read("/tmp/foo.rs", "hello", ts(123));
        let state = cache.state(Path::new("/tmp/foo.rs")).unwrap();
        assert_eq!(state.modified_at, ts(123));
        assert_eq!(state.content_hash, hash_content("hello"));
    }

    #[tokio::test]
    async fn shared_cache_supports_concurrent_access() {
        let cache: SharedFileStateCache = Arc::new(RwLock::new(FileStateCache::new()));

        cache
            .write()
            .await
            .record_read("/tmp/a.rs", "hi", ts(1));
        cache
            .write()
            .await
            .record_read("/tmp/b.rs", "bye", ts(2));

        let guard = cache.read().await;
        assert_eq!(guard.len(), 2);
        assert!(!guard.is_modified(Path::new("/tmp/a.rs"), "hi", ts(1)));
        assert!(guard.is_modified(Path::new("/tmp/b.rs"), "changed", ts(2)));
    }
}
