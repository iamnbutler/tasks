//! Human presence tracking — spec Section 4.1.
//!
//! The server tracks whether a GUI client is connected.
//! - Connected = human is present, questions surface for timely response.
//! - Disconnected = autonomous mode, orchestrator makes judgment calls.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Tracks active GUI connections to determine human presence.
pub struct PresenceTracker {
    active_connections: AtomicUsize,
}

impl PresenceTracker {
    pub fn new() -> Self {
        Self {
            active_connections: AtomicUsize::new(0),
        }
    }

    /// A GUI client connected.
    pub fn connect(&self) -> ConnectionGuard<'_> {
        self.active_connections.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard { tracker: self }
    }

    /// A GUI client connected (owned version).
    ///
    /// Returns an owned guard that doesn't borrow the tracker.
    /// Use this when the guard needs to outlive a function scope,
    /// e.g., when captured by an async stream.
    pub fn connect_owned(self: &Arc<Self>) -> OwnedConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::SeqCst);
        OwnedConnectionGuard {
            tracker: Arc::clone(self),
        }
    }

    /// Whether any GUI client is connected (human is present).
    pub fn is_present(&self) -> bool {
        self.active_connections.load(Ordering::SeqCst) > 0
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }

    fn disconnect(&self) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that decrements the connection count on drop.
pub struct ConnectionGuard<'a> {
    tracker: &'a PresenceTracker,
}

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.tracker.disconnect();
    }
}

/// Owned RAII guard that decrements the connection count on drop.
///
/// Unlike [`ConnectionGuard`], this holds an `Arc` and can be moved
/// into async streams or other contexts that outlive the function scope.
pub struct OwnedConnectionGuard {
    tracker: Arc<PresenceTracker>,
}

impl Drop for OwnedConnectionGuard {
    fn drop(&mut self) {
        self.tracker.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_tracking() {
        let tracker = PresenceTracker::new();
        assert!(!tracker.is_present());

        let guard1 = tracker.connect();
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        let guard2 = tracker.connect();
        assert_eq!(tracker.connection_count(), 2);

        drop(guard1);
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        drop(guard2);
        assert!(!tracker.is_present());
    }

    #[test]
    fn owned_presence_tracking() {
        let tracker = Arc::new(PresenceTracker::new());
        assert!(!tracker.is_present());

        let guard1 = tracker.connect_owned();
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        let guard2 = tracker.connect_owned();
        assert_eq!(tracker.connection_count(), 2);

        drop(guard1);
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        drop(guard2);
        assert!(!tracker.is_present());
    }

    #[test]
    fn mixed_guards() {
        let tracker = Arc::new(PresenceTracker::new());
        assert!(!tracker.is_present());

        let borrowed = tracker.connect();
        let owned = tracker.connect_owned();
        assert_eq!(tracker.connection_count(), 2);

        drop(borrowed);
        assert_eq!(tracker.connection_count(), 1);

        drop(owned);
        assert_eq!(tracker.connection_count(), 0);
        assert!(!tracker.is_present());
    }
}
