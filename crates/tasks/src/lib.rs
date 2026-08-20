//! tasks: human-in-the-loop agent orchestration server.
//!
//! Domain and wire types live in the `tasks-api` crate (re-exported here as
//! `models` and `events`) so native clients can share them without depending
//! on the server stack.

/// GitHub sign-in by the OAuth device flow — `tasks auth login` (#1002).
pub mod auth;
pub mod brief;
/// Short-lived credential leases and the proxy that redeems them, so VMs
/// operate on credit and never hold a raw key (#971).
pub mod broker;
/// Whether the broker is answering, and whether dispatch should wait for it —
/// the fourth standing hold (#1006).
pub mod broker_health;
pub mod builder;
pub mod bundles;
pub mod cancel;
/// Run budgets measured on both the monotonic and the wall clock, so a
/// suspended host reads as a suspend and not as a timeout (#929).
pub mod deadline;
/// What the scout dispatcher may start and whether it may start it, asked
/// together — so the hold read cannot be dropped from the dispatch (#973).
pub mod dispatch_gate;
/// Every precondition for a scout, asked at once: `tasks doctor` (#990).
pub mod doctor;
pub mod env_file;
pub mod github;
/// Whether GitHub is answering, and whether dispatch should wait for it (#939).
pub mod github_health;
/// What the VM images are running, observed from the runs inside them (#909).
pub mod images;
/// The two rules that keep a web page you visit from driving the local API:
/// the authority must be this machine's loopback, and an `Origin` header is a
/// refusal (#985).
pub mod loopback;
/// The migration set plus the naming rule that keeps parallel branches from
/// colliding on it — private, like `teardown`: the store is the only caller.
mod migrations;
pub mod orchestrator;
pub mod pidfile;
/// Whether vm-pool has a slot to give, and whether dispatch should wait for
/// one (#967).
pub mod pool_health;
pub mod reattach;
pub mod redact;
pub mod reload;
pub mod run;
/// Whether this host can start a container at all, and whether dispatch
/// should wait for it — the fifth standing hold (#1017).
pub mod runtime_health;
pub mod scout;
/// Sealed at-rest storage for the two upstream keys, and the live handle the
/// server reads them through (#971).
pub mod secrets;
pub mod server;
pub mod service;
pub mod store;
mod teardown;
pub mod transcript;
pub mod updates;
pub mod verify_dir;
pub mod version;
pub mod viewer;
pub mod worker;

pub use tasks_api::{events, models};
pub use tasks_protocol as protocol;
