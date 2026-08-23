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

//! Two modules here are not wire types, for one reason. `paths` is the
//! filesystem record a server publishes about itself
//! (`<data dir>/tasks.pid`), and `first_play` is the record that a human has
//! been told what `play` does on this install (`<data dir>/first-play.json`)
//! plus the grouping of the charter that sheet renders. Both live here because
//! more than one client reads them, so there is one definition rather than a
//! copy per client — and because `app-gpui` is not a workspace member, so a
//! rule left in the app is a rule `make test` never runs.

pub mod events;
pub mod first_play;
pub mod http;
pub mod models;
pub mod paths;
pub mod version;
