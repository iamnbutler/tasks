//! This client's build, and the verdict on it.
//!
//! A stale client used to fail as a pile of decode errors — a strict enum that
//! didn't know a variant, a struct missing a field — which reads as "the
//! server is broken" when the actual fact is "your app is old". [`Preflight`]
//! turns that into one sentence, asked once on connect.

use std::cmp::Ordering;

use tasks_api::version::{Support, VersionInfo, compare};

/// `0.1.<commit count>` for this build of `tasks-client`, or the crate version
/// with no git in reach. An embedder that ships its own stamp (the app's
/// About version) should pass that to [`Client::with_client_version`] instead,
/// so the warning names the number the user can actually read off the screen.
///
/// [`Client::with_client_version`]: crate::Client::with_client_version
pub const CLIENT_VERSION: &str = env!("TASKS_CLIENT_VERSION");

/// Short SHA (`-dirty` when the tree had uncommitted changes), or `unknown`.
pub const CLIENT_COMMIT: &str = env!("TASKS_CLIENT_COMMIT");

/// What a connect-time version check found.
///
/// [`Preflight::warning`] is the whole client-side API in the common case:
/// put it in a banner and be done. Every variant that cannot say something
/// certain and actionable returns `None` there, on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// This client is at or above the server's floor. Nothing to say.
    Current { server: VersionInfo, client: String },
    /// This client is below the server's floor. It is still served — every
    /// route keeps answering — but whatever breaks next is explained by this.
    Outdated { server: VersionInfo, client: String },
    /// One of the two builds doesn't carry a parseable version (no git in
    /// reach when it was built). Nothing certain to report, so nothing is
    /// reported: a warning that fires on merely *unidentifiable* builds gets
    /// trained out of use, and then the real one goes unread.
    Indeterminate { server: VersionInfo, client: String },
    /// The server answered 404 for `/version` — it predates the route, so
    /// *it* is the stale end of this pair. The only reverse-skew signal there
    /// is today; there is no `min_server_version`.
    ServerUnversioned { client: String },
}

impl Preflight {
    pub(crate) fn judge(client_version: &str, server: VersionInfo) -> Self {
        match server.supports(client_version) {
            Support::Current => Preflight::Current {
                server,
                client: client_version.to_string(),
            },
            Support::TooOld => Preflight::Outdated {
                server,
                client: client_version.to_string(),
            },
            Support::Unknown => Preflight::Indeterminate {
                server,
                client: client_version.to_string(),
            },
        }
    }

    /// The one line a UI shows, or `None` when there is nothing worth saying.
    pub fn warning(&self) -> Option<String> {
        match self {
            Preflight::Current { .. } | Preflight::Indeterminate { .. } => None,
            Preflight::Outdated { server, client } => Some(format!(
                "This client build ({client}) is older than the server supports \
                 (needs {}, server is {}) — rebuild the client (`make app`).",
                server.min_client_version, server.version
            )),
            Preflight::ServerUnversioned { client } => Some(format!(
                "This server is older than this client build ({client}): it has no \
                 /version route — restart the server from a current build."
            )),
        }
    }

    /// The server's identity, when it published one.
    pub fn server(&self) -> Option<&VersionInfo> {
        match self {
            Preflight::Current { server, .. }
            | Preflight::Outdated { server, .. }
            | Preflight::Indeterminate { server, .. } => Some(server),
            Preflight::ServerUnversioned { .. } => None,
        }
    }

    /// The client version this verdict was rendered against.
    pub fn client_version(&self) -> &str {
        match self {
            Preflight::Current { client, .. }
            | Preflight::Outdated { client, .. }
            | Preflight::Indeterminate { client, .. }
            | Preflight::ServerUnversioned { client } => client,
        }
    }

    /// Whether this client is known to be under the server's floor.
    pub fn is_outdated(&self) -> bool {
        matches!(self, Preflight::Outdated { .. })
    }

    /// Whether the client is *ahead* of the server's build — you rebuilt the
    /// app but forgot to restart the server. Not a floor violation and not a
    /// warning today; the data is here for whoever wants it.
    pub fn client_ahead_of_server(&self) -> bool {
        match self {
            Preflight::ServerUnversioned { .. } => true,
            _ => self.server().is_some_and(|server| {
                compare(self.client_version(), &server.version) == Some(Ordering::Greater)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(min: &str) -> VersionInfo {
        VersionInfo {
            version: "0.1.163".into(),
            commit: "abc1234".into(),
            min_client_version: min.into(),
        }
    }

    #[test]
    fn current_client_says_nothing() {
        let verdict = Preflight::judge("0.1.163", server("0.1.140"));
        assert!(matches!(verdict, Preflight::Current { .. }));
        assert_eq!(verdict.warning(), None);
    }

    #[test]
    fn outdated_client_names_all_three_numbers() {
        let verdict = Preflight::judge("0.1.120", server("0.1.140"));
        let warning = verdict.warning().expect("a stale client is worth a line");
        assert!(warning.contains("0.1.120"), "{warning}");
        assert!(warning.contains("0.1.140"), "{warning}");
        assert!(warning.contains("0.1.163"), "{warning}");
        assert!(verdict.is_outdated());
        assert_eq!(verdict.client_version(), "0.1.120");
        assert_eq!(
            verdict.server().map(|s| s.version.as_str()),
            Some("0.1.163")
        );
    }

    /// The count crosses digit boundaries constantly; a lexical comparison
    /// would call 0.1.9 newer than 0.1.100 and warn on a current build.
    #[test]
    fn floor_comparison_is_numeric() {
        assert!(!Preflight::judge("0.1.100", server("0.1.9")).is_outdated());
        assert!(Preflight::judge("0.1.9", server("0.1.100")).is_outdated());
    }

    #[test]
    fn unidentifiable_build_does_not_warn() {
        let verdict = Preflight::judge("unknown", server("0.1.140"));
        assert!(matches!(verdict, Preflight::Indeterminate { .. }));
        assert_eq!(verdict.warning(), None);
    }

    #[test]
    fn unversioned_server_is_the_stale_one() {
        let verdict = Preflight::ServerUnversioned {
            client: "0.1.163".into(),
        };
        let warning = verdict.warning().expect("an old server is worth a line");
        assert!(warning.contains("0.1.163"), "{warning}");
        assert!(warning.contains("server"), "{warning}");
        assert_eq!(verdict.server(), None);
        assert!(verdict.client_ahead_of_server());
    }

    #[test]
    fn reverse_skew_is_visible_without_being_a_warning() {
        let verdict = Preflight::judge("0.1.200", server("0.1.140"));
        assert_eq!(
            verdict.warning(),
            None,
            "a newer client is not warned about"
        );
        assert!(verdict.client_ahead_of_server());
    }
}
