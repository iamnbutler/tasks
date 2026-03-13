//! Session runtime for Tasks platform.
//!
//! Manages container-based sessions for agent execution. Each session gets
//! an isolated container with its own copy of the repo and a running agent.

pub mod protocol;
mod container;
mod transport;
mod session;

pub use container::{ContainerConfig, ContainerError, ContainerRuntime, AppleContainerRuntime};
pub use transport::{StdioTransport, TransportError};
pub use session::{Session, SessionError, SessionState};
