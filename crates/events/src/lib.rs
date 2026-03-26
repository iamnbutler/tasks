//! Event system for Tasks platform.
//!
//! Provides an append-only event log with in-memory pub/sub for live subscriptions.
//! Events are persisted per-task as JSONL files.

mod event;
mod store;
mod bus;

pub use event::{Event, EventType, Actor};
pub use store::{EventStore, RetentionPolicy, StoreError, EVENT_FORMAT_VERSION};
pub use bus::{EventBus, matches_pattern, matches_task};
