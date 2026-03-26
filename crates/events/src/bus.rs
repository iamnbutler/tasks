//! In-memory event pub/sub.
//!
//! Sits in front of the event store for live subscriptions.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::{Event, EventStore, store::StoreError};

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

    /// Query events across all tasks by event-type prefix.
    ///
    /// See [`EventStore::query_by_type_prefix`] for details.
    pub async fn query_by_type_prefix(
        &self,
        type_prefix: &str,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.store.query_by_type_prefix(type_prefix, limit).await
    }

    /// List all task IDs with event logs.
    pub async fn list_tasks(&self) -> Result<Vec<String>, StoreError> {
        self.store.list_tasks().await
    }

    /// Compact all task event logs according to the retention policy.
    ///
    /// Returns the total number of events removed.
    pub async fn compact(&self) -> Result<usize, StoreError> {
        self.store.compact_all().await
    }

    /// Remove orphaned (empty) task directories.
    ///
    /// Returns the number of directories removed.
    pub async fn cleanup_orphaned_tasks(&self) -> Result<usize, StoreError> {
        self.store.cleanup_orphaned_tasks().await
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
    async fn filter_live_events_by_pattern() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        let mut rx = bus.subscribe();

        // Publish events of different types
        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        bus.publish(Event::new(
            EventType::AgentMessage,
            "task-1",
            Actor::Agent,
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

        // Collect and filter for task events only
        let mut task_events = Vec::new();
        for _ in 0..3 {
            let event = rx.recv().await.unwrap();
            if matches_pattern(&event, "task:*") {
                task_events.push(event);
            }
        }

        assert_eq!(task_events.len(), 2);
        assert!(matches_pattern(&task_events[0], "task:created"));
        assert!(matches_pattern(&task_events[1], "task:state:running"));
    }

    #[tokio::test]
    async fn filter_live_events_by_task_id() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        let mut rx = bus.subscribe();

        // Publish events for different tasks
        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-2",
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

        // Collect and filter for task-1 only
        let mut task1_events = Vec::new();
        for _ in 0..3 {
            let event = rx.recv().await.unwrap();
            if matches_task(&event, "task-1") {
                task1_events.push(event);
            }
        }

        assert_eq!(task1_events.len(), 2);
    }

    #[tokio::test]
    async fn multiple_subscribers_independent_filtering() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        // Publish mixed events
        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        bus.publish(Event::new(
            EventType::AgentMessage,
            "task-1",
            Actor::Agent,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        // Subscriber 1 filters for task events
        let mut task_events = Vec::new();
        for _ in 0..2 {
            let event = rx1.recv().await.unwrap();
            if matches_pattern(&event, "task:*") {
                task_events.push(event);
            }
        }

        // Subscriber 2 filters for agent events
        let mut agent_events = Vec::new();
        for _ in 0..2 {
            let event = rx2.recv().await.unwrap();
            if matches_pattern(&event, "agent:*") {
                agent_events.push(event);
            }
        }

        assert_eq!(task_events.len(), 1);
        assert_eq!(agent_events.len(), 1);
    }

    #[test]
    fn pattern_matching_negative_cases() {
        let task_event = Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );

        // Should not match unrelated patterns
        assert!(!matches_pattern(&task_event, "agent:*"));
        assert!(!matches_pattern(&task_event, "merge:*"));
        assert!(!matches_pattern(&task_event, "system:*"));
        assert!(!matches_pattern(&task_event, "orchestrator:*"));
        assert!(!matches_pattern(&task_event, "human:*"));

        // Should not match partial patterns (no wildcard)
        assert!(!matches_pattern(&task_event, "task"));
        assert!(!matches_pattern(&task_event, "task:"));
        assert!(!matches_pattern(&task_event, "task:state:running"));

        // Should not match wrong exact value
        assert!(!matches_pattern(&task_event, "task:state:completed"));
    }

    #[test]
    fn pattern_matching_all_event_families() {
        let events = vec![
            (EventType::TaskCreated, "task:*"),
            (EventType::AgentMessage, "agent:*"),
            (EventType::MergeQueued, "merge:*"),
            (EventType::OrchestratorFeedback, "orchestrator:*"),
            (EventType::SystemStarted, "system:*"),
            (EventType::HumanMessage, "human:*"),
        ];

        for (event_type, pattern) in events {
            let event = Event::new(event_type, "task-1", Actor::System, serde_json::json!({}));
            assert!(
                matches_pattern(&event, pattern),
                "Expected {:?} to match {}",
                event.event_type,
                pattern
            );
        }
    }

    #[test]
    fn filter_combined_pattern_and_task() {
        let event1 = Event::new(
            EventType::TaskStateRunning,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        );
        let event2 = Event::new(
            EventType::TaskStateRunning,
            "task-2",
            Actor::System,
            serde_json::json!({}),
        );
        let event3 = Event::new(
            EventType::AgentMessage,
            "task-1",
            Actor::Agent,
            serde_json::json!({}),
        );

        // Only event1 matches both criteria
        let matches_both = |e: &Event| matches_pattern(e, "task:state:*") && matches_task(e, "task-1");

        assert!(matches_both(&event1));
        assert!(!matches_both(&event2)); // wrong task
        assert!(!matches_both(&event3)); // wrong pattern
    }

    #[test]
    fn filter_star_matches_everything() {
        let events = vec![
            Event::new(EventType::TaskCreated, "task-1", Actor::System, serde_json::json!({})),
            Event::new(EventType::AgentMessage, "task-2", Actor::Agent, serde_json::json!({})),
            Event::new(EventType::MergeCompleted, "task-3", Actor::System, serde_json::json!({})),
            Event::new(EventType::SystemStarted, "", Actor::System, serde_json::json!({})),
        ];

        for event in &events {
            assert!(
                matches_pattern(event, "*"),
                "Expected {:?} to match *",
                event.event_type
            );
        }
    }

    #[test]
    fn filter_empty_task_id() {
        let event = Event::new(
            EventType::SystemStarted,
            "",
            Actor::System,
            serde_json::json!({}),
        );

        assert!(matches_task(&event, ""));
        assert!(!matches_task(&event, "task-1"));
    }

    #[tokio::test]
    async fn filter_replay_events() {
        let dir = tempdir().unwrap();
        let store = EventStore::new(dir.path());
        let bus = EventBus::new(store, 16);

        // Publish historical events
        bus.publish(Event::new(
            EventType::TaskCreated,
            "task-1",
            Actor::System,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        bus.publish(Event::new(
            EventType::AgentMessage,
            "task-1",
            Actor::Agent,
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

        // Subscribe with replay and filter historical events
        let (history, _rx) = bus.subscribe_with_replay("task-1").await.unwrap();

        let task_state_events: Vec<_> = history
            .iter()
            .filter(|e| matches_pattern(e, "task:state:*"))
            .collect();

        assert_eq!(task_state_events.len(), 1);
        assert!(matches_pattern(task_state_events[0], "task:state:running"));
    }
}
