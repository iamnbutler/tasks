//! In-memory event pub/sub.
//!
//! Sits in front of the event store for live subscriptions.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::{Event, EventStore, store::StoreError};

/// A pattern-filtered event subscriber (spec §12.1).
///
/// Wraps a broadcast receiver and filters events based on patterns.
/// Created via [`EventBus::subscribe_with_patterns`].
pub struct PatternSubscriber {
    receiver: broadcast::Receiver<Arc<Event>>,
    patterns: Vec<String>,
}

impl PatternSubscriber {
    /// Receive the next event that matches any of the patterns.
    ///
    /// Blocks until a matching event is available or the channel is closed.
    pub async fn recv(&mut self) -> Result<Arc<Event>, broadcast::error::RecvError> {
        loop {
            let event = self.receiver.recv().await?;
            if self.matches(&event) {
                return Ok(event);
            }
        }
    }

    /// Check if an event matches any of the configured patterns.
    fn matches(&self, event: &Event) -> bool {
        self.patterns.iter().any(|p| event.event_type.matches(p))
    }
}

/// Event bus combining storage with live pub/sub.
///
/// Events are persisted to the store and broadcast to subscribers.
/// Subscribers can also replay historical events from the store.
pub struct EventBus {
    store: EventStore,
    sender: broadcast::Sender<Arc<Event>>,
}

impl EventBus {
    /// Create a new event bus with the given store.
    ///
    /// `capacity` is the broadcast channel buffer size for live events.
    pub fn new(store: EventStore, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { store, sender }
    }

    /// Publish an event: persist to store and broadcast to subscribers.
    pub async fn publish(&self, event: Event) -> Result<(), StoreError> {
        self.store.append(&event).await?;
        if let Err(e) = self.sender.send(Arc::new(event)) {
            tracing::debug!(event_type = %e.0.event_type.as_str(), "no subscribers for broadcast event");
        }
        Ok(())
    }

    /// Subscribe to live events.
    ///
    /// Returns a receiver that will get all events published after this call.
    /// Use `subscribe_with_replay` to also get historical events.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.sender.subscribe()
    }

    /// Subscribe to events matching specific patterns (spec §12.1).
    ///
    /// Returns a [`PatternSubscriber`] that filters events based on the given patterns.
    /// Patterns support wildcards: `task:*` matches all task events,
    /// `task:state:*` matches all state changes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut sub = event_bus.subscribe_with_patterns(&["task:created", "task:state:*"]);
    /// while let Ok(event) = sub.recv().await {
    ///     // Only receives task:created and task:state:* events
    /// }
    /// ```
    pub fn subscribe_with_patterns(&self, patterns: &[&str]) -> PatternSubscriber {
        PatternSubscriber {
            receiver: self.sender.subscribe(),
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Subscribe to events for a specific task, replaying history first.
    ///
    /// Returns historical events and a receiver for live events.
    pub async fn subscribe_with_replay(
        &self,
        task_id: &str,
    ) -> Result<(Vec<Event>, broadcast::Receiver<Arc<Event>>), StoreError> {
        // Get historical events first
        let history = self.store.read_task(task_id).await?;
        // Then subscribe to live events
        let receiver = self.sender.subscribe();
        Ok((history, receiver))
    }

    /// Read historical events for a task.
    pub async fn read_task(&self, task_id: &str) -> Result<Vec<Event>, StoreError> {
        self.store.read_task(task_id).await
    }

    /// List all task IDs with event logs.
    pub async fn list_tasks(&self) -> Result<Vec<String>, StoreError> {
        self.store.list_tasks().await
    }
}

/// Filter events by pattern.
///
/// Use with a subscription receiver to filter for specific event types.
pub fn matches_pattern(event: &Event, pattern: &str) -> bool {
    event.event_type.matches(pattern)
}

/// Filter events by task ID.
pub fn matches_task(event: &Event, task_id: &str) -> bool {
    event.task == task_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Actor, EventType};
    use tempfile::tempdir;

    #[tokio::test]
    async fn publish_and_subscribe() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        let mut rx = bus.subscribe();

        let event = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );
        let event_id = event.id;

        bus.publish(event).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.id, event_id);
    }

    #[tokio::test]
    async fn subscribe_with_replay() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        // Publish some events before subscribing
        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        bus.publish(Event::new(
            EventType::TaskStateRunning,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        // Subscribe with replay
        let (history, _rx) = bus.subscribe_with_replay("task-1").await.unwrap();
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn filter_by_pattern() {
        let event = Event::new(
            EventType::TaskStateRunning,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );

        assert!(matches_pattern(&event, "task:*"));
        assert!(matches_pattern(&event, "task:state:*"));
        assert!(matches_pattern(&event, "task:state:running"));
        assert!(!matches_pattern(&event, "agent:*"));
    }

    #[test]
    fn filter_by_task() {
        let event = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );

        assert!(matches_task(&event, "task-1"));
        assert!(!matches_task(&event, "task-2"));
    }

    #[tokio::test]
    async fn subscribe_with_patterns_filters_events() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        // Subscribe only to task:created and task:state:* events
        let mut sub = bus.subscribe_with_patterns(&["task:created", "task:state:*"]);

        // Publish events of different types
        let task_created = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );
        let task_created_id = task_created.id;

        let agent_message = Event::new(
            EventType::AgentMessage,
            "task-1",
            Actor::Agent,
            serde_json::json!({}),
        );

        let task_running = Event::new(
            EventType::TaskStateRunning,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );
        let task_running_id = task_running.id;

        bus.publish(task_created).await.unwrap();
        bus.publish(agent_message).await.unwrap();
        bus.publish(task_running).await.unwrap();

        // Should receive task:created (matches exact pattern)
        let received = sub.recv().await.unwrap();
        assert_eq!(received.id, task_created_id);

        // Should skip agent:message and receive task:state:running (matches task:state:*)
        let received = sub.recv().await.unwrap();
        assert_eq!(received.id, task_running_id);
    }
}
