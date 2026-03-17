//! Tasks Desktop — GPUI-based desktop application.
//!
//! This crate provides a native desktop UI for the Tasks platform,
//! built with GPUI (Zed's GPU-accelerated UI framework).

mod sse;

pub use sse::{SseClient, SseClientEvent, SseConnectionState, SseFilters};
