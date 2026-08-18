//! Credential scrubbing — re-exported from [`tasks_protocol::redact`].
//!
//! The implementation moved down to `tasks-protocol` so that both supervisors,
//! which mint the same credentialed clone URL from inside a VM and do not
//! depend on this crate, can share it. This shim keeps `crate::redact::…`
//! working at the call sites (and the doc comments elsewhere that point at
//! them); the rules, and the argument for where they live, are in the module
//! it re-exports.

pub use tasks_protocol::redact::{Secret, redact, redact_line, redact_owned};
