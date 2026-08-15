//! Wire types for the tasks HTTP API.
//!
//! Everything a client needs to talk to the server with the server's own
//! types: domain models (`models`), the append-only event vocabulary
//! (`events`), and the request/response bodies that aren't themselves domain
//! types (`http`). Deliberately dependency-light — serde, chrono, uuid —
//! so a GUI client can depend on it without dragging in the server stack.
//!
//! Clients and server ship from this repo and share these exact types, so
//! enums are strict: an unknown variant is a deserialization error, not a
//! lenient fallback. Version skew between a dev client and a dev server is
//! a build error here, on purpose.

pub mod events;
pub mod http;
pub mod models;
pub mod version;
