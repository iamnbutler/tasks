//! Host ↔ container supervisor protocol.
//!
//! JSON-line protocol over stdio. Commands flow host→container,
//! events flow container→host.

mod commands;
mod events;
mod codec;

pub use commands::*;
pub use events::*;
pub use codec::{encode, decode_line, LineReader};
