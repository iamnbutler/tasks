//! Human presence tracking — spec Section 4.1.
//!
//! The server tracks whether a GUI client is connected.
//! - Connected = human is present, questions surface for timely response.
//! - Disconnected = autonomous mode, orchestrator makes judgment calls.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};

/// Tracks active GUI connections to determine human presence.
pub struct PresenceTracker {
    active_connections: AtomicUsize,
    /// Timestamp of the last disconnect (when connections went from >0 to 0).
    /// Protected by a mutex since it's only written on rare transitions.
    last_disconnect_at: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl PresenceTracker {
    pub fn new() -> Self {
        Self {
            active_connections: AtomicUsize::new(0),
            last_disconnect_at: std::sync::Mutex::new(None),
        }
    }

    /// A GUI client connected.
    ///
    /// Returns `(guard, was_reconnect)` — `was_reconnect` is true if this
    /// was a transition from 0 to 1 connections (human just returned).
    pub fn connect(&self) -> (ConnectionGuard<'_>, bool) {
        let prev = self.active_connections.fetch_add(1, Ordering::SeqCst);
        (ConnectionGuard { tracker: self }, prev == 0)
    }

    /// A GUI client connected (owned version).
    ///
    /// Returns `(guard, was_reconnect)` — `was_reconnect` is true if this
    /// was a transition from 0 to 1 connections (human just returned).
    /// Use this when the guard needs to outlive a function scope,
    /// e.g., when captured by an async stream.
    pub fn connect_owned(self: &Arc<Self>) -> (OwnedConnectionGuard, bool) {
        let prev = self.active_connections.fetch_add(1, Ordering::SeqCst);
        (OwnedConnectionGuard {
            tracker: Arc::clone(self),
        }, prev == 0)
    }

    /// Whether any GUI client is connected (human is present).
    pub fn is_present(&self) -> bool {
        self.active_connections.load(Ordering::SeqCst) > 0
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }

    /// Returns true if this disconnect was a transition from 1 to 0 (human left).
    fn disconnect(&self) -> bool {
        let prev = self.active_connections.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            // Transition from connected to disconnected
            if let Ok(mut last) = self.last_disconnect_at.lock() {
                *last = Some(Utc::now());
            }
            true
        } else {
            false
        }
    }

    /// When the human last disconnected (all connections dropped).
    pub fn last_disconnect_at(&self) -> Option<DateTime<Utc>> {
        self.last_disconnect_at.lock().ok().and_then(|v| *v)
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

        let (guard1, was_reconnect) = tracker.connect();
        assert!(was_reconnect); // first connection is a reconnect
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        let (guard2, was_reconnect2) = tracker.connect();
        assert!(!was_reconnect2); // second connection is not a reconnect
        assert_eq!(tracker.connection_count(), 2);

        drop(guard1);
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        drop(guard2);
        assert!(!tracker.is_present());
        assert!(tracker.last_disconnect_at().is_some());
    }

    #[test]
    fn owned_presence_tracking() {
        let tracker = Arc::new(PresenceTracker::new());
        assert!(!tracker.is_present());

        let (guard1, was_reconnect) = tracker.connect_owned();
        assert!(was_reconnect);
        assert!(tracker.is_present());
        assert_eq!(tracker.connection_count(), 1);

        let (guard2, _) = tracker.connect_owned();
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

        let (borrowed, _) = tracker.connect();
        let (owned, _) = tracker.connect_owned();
        assert_eq!(tracker.connection_count(), 2);

        drop(borrowed);
        assert_eq!(tracker.connection_count(), 1);

        drop(owned);
        assert_eq!(tracker.connection_count(), 0);
        assert!(!tracker.is_present());
    }
}
