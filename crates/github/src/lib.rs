//! GitHub integration crate for the Tasks platform.
//!
//! Provides a GraphQL client for fetching issues and pull requests from GitHub,
//! a normalized model decoupled from GitHub's API shapes, and a polling
//! interface for discovering new and changed work.
//!
//! See `spec/github.md` for the full specification.

pub mod client;
pub mod error;
pub mod model;
pub mod poller;

mod queries;
mod response;

pub use client::{CreatedComment, CreatedIssue, GitHubClient, UpdatedIssue};
pub use error::GitHubError;
pub use poller::{PollResult, RepoPoller};
