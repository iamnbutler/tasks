//! Session management for the Tasks platform.
//!
//! Manages active container sessions — the bridge between dispatcher
//! decisions and running agent containers. See spec §9.

mod accounting;
mod interpreter;
mod manager;

pub use accounting::{TokenParser, TokenTracker, TokenUsage};
pub use interpreter::{OutputInterpreter, OutputSignal, emit_signal_events};
pub use manager::{ContainerInfo, SessionLimits, SessionManager, SessionManagerError, SessionHandle};
