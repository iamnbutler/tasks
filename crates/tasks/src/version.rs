//! What build this server is, and the oldest client it expects to talk to.
//!
//! Served by `GET /version` — deliberately store-free, so it answers while the
//! rest of the process is still opening its database.

use tasks_api::version::VersionInfo;

/// `0.1.<commit count>`, stamped by `build.rs`. `0.1.0-alpha.1` (the crate
/// version) means the binary was built without git in reach.
pub const VERSION: &str = env!("TASKS_SERVER_VERSION");

/// Short SHA, `-dirty` when the tree had uncommitted changes, or `unknown`.
pub const COMMIT: &str = env!("TASKS_SERVER_COMMIT");

/// The oldest client build this server still expects to speak to.
///
/// **Move this by hand, and only for a wire change** — a route a client of
/// that vintage doesn't know about, a field it will fail to decode, a
/// semantic it will get wrong. It deliberately does not follow [`VERSION`]:
/// a floor equal to the current build declares every client stale the moment
/// the server is rebuilt, which conveys nothing and trains the warning out of
/// use. Raising it is a claim that older clients are actually broken, and the
/// only cost of leaving it low is that a genuinely-broken old client says
/// nothing instead of saying so.
pub const MIN_CLIENT_VERSION: &str = "0.1.0";

/// This build's identity, as it goes on the wire.
pub fn info() -> VersionInfo {
    VersionInfo {
        version: VERSION.to_string(),
        commit: COMMIT.to_string(),
        min_client_version: MIN_CLIENT_VERSION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    #[test]
    fn info_reports_the_stamped_build() {
        let info = info();
        assert_eq!(info.version, VERSION);
        assert_eq!(info.commit, COMMIT);
        assert_eq!(info.min_client_version, MIN_CLIENT_VERSION);
        assert!(!info.version.is_empty() && !info.commit.is_empty());
    }

    /// A floor *ahead* of the running build is the tell that it was raised as
    /// a ratchet ("bump it with the version") rather than for an actual wire
    /// break — and it would declare every client, including one built from
    /// this very commit, too old.
    #[test]
    fn floor_is_never_ahead_of_this_build() {
        let Some(ordering) = tasks_api::version::compare(MIN_CLIENT_VERSION, VERSION) else {
            // No git in reach: VERSION is the crate version and there is
            // nothing meaningful to compare against.
            return;
        };
        assert_ne!(
            ordering,
            Ordering::Greater,
            "MIN_CLIENT_VERSION ({MIN_CLIENT_VERSION}) is ahead of this build ({VERSION}): \
             every client, including one built from this commit, would be told it is stale"
        );
    }

    #[test]
    fn this_build_meets_its_own_floor() {
        assert_ne!(
            info().supports(VERSION),
            tasks_api::version::Support::TooOld
        );
    }
}
