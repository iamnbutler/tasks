//! Session management for the Tasks platform.
//!
//! Manages active container sessions — the bridge between dispatcher
//! decisions and running agent containers. See spec §9.

mod manager;
pub mod token_parser;

pub use manager::{SessionManager, SessionManagerError, SessionHandle};
