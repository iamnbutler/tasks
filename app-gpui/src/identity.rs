//! The signed-in human, as the chat chip renders them.
//!
//! Pure view state over `GET /viewer` — no gpui, no client, so every state is
//! unit-testable without a window. The server's wire enum has three
//! ([`Viewer::Known`], `Unauthenticated`, `Unknown`); the app needs a fourth,
//! and `Option::None` is it: **not answered yet**. That is neither verdict and
//! must not render as "Not signed in", or every correctly configured app
//! flashes that under the cursor on launch.
//!
//! [`should_ask_for_viewer`] is the other half, and it is asymmetric on
//! purpose. The fallback states are asked about on **every** refresh, so a
//! token sealed after the app opened arrives on the next SSE event — that is
//! the case that actually happens, a fresh machine being configured, and it
//! costs no GitHub traffic either way (the server answers "no credential"
//! without asking, and caches a failure). A *settled* identity is asked about
//! again on a coarse interval, because the alternative — asking once and never
//! again — leaves an app that is open across a token rotation showing the
//! previous account's face and linking to their profile indefinitely. The
//! interval is cheap: the server caches a success for half an hour, so a valid
//! token costs one GitHub call per 30 minutes however often this asks.

use std::time::Duration;

use tasks_client::api::http::Viewer;

/// How long a settled identity stands before the app asks the server again.
///
/// Coarse on purpose. This is a chip's avatar, and the server's own cache
/// absorbs the traffic — what the interval buys is that a token rotated to
/// another account on a running server is picked up without a restart, which
/// is also what makes [`crate::workspace::Workspace`]'s changed-avatar branch
/// reachable rather than dead code claiming a rotation is handled.
pub(crate) const VIEWER_RECHECK: Duration = Duration::from_secs(15 * 60);

/// What the chip renders: a sentence for the tooltip, and somewhere to go if
/// there is anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChipIdentity {
    /// Shown under the cursor. Always says which of the four states this is —
    /// a placeholder circle with no explanation is the thing #987 left behind.
    pub(crate) tooltip: String,
    /// `Some` only for [`Viewer::Known`]. `None` is what makes the chip inert:
    /// no pointer cursor, no click, and therefore no link to a stranger's
    /// GitHub or to nowhere at all.
    pub(crate) profile_url: Option<String>,
}

/// The chip's identity for one answer — including no answer yet.
pub(crate) fn chip_identity(viewer: Option<&Viewer>) -> ChipIdentity {
    match viewer {
        Some(v @ Viewer::Known { .. }) => ChipIdentity {
            tooltip: v.describe(),
            profile_url: v.profile_url().map(str::to_owned),
        },
        Some(v) => ChipIdentity {
            tooltip: v.describe(),
            profile_url: None,
        },
        // The fourth state. Before the first snapshot lands there is no verdict
        // to report, and reporting one of the other three would be a guess.
        None => ChipIdentity {
            tooltip: "Checking GitHub identity…".to_string(),
            profile_url: None,
        },
    }
}

/// Whether the next refresh should ask `GET /viewer`.
///
/// `answered` is how long ago the last answer landed — `None` when none ever
/// has. Anything but a settled `Known` is asked for again immediately; a
/// settled one waits out [`VIEWER_RECHECK`].
pub(crate) fn should_ask_for_viewer(viewer: Option<&Viewer>, answered: Option<Duration>) -> bool {
    match viewer {
        Some(Viewer::Known { .. }) => answered.is_none_or(|since| since >= VIEWER_RECHECK),
        // No answer yet, no credential, or a failure: keep asking. None of the
        // three costs a GitHub call the server would not otherwise make.
        _ => true,
    }
}

/// Where to fetch the avatar image, when there is one to fetch.
///
/// `None` in all three of the other states, which the chip renders as its
/// existing placeholder circle — same footprint, so nothing reflows.
pub(crate) fn avatar_url(viewer: Option<&Viewer>) -> Option<String> {
    viewer.and_then(Viewer::avatar_url).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Viewer {
        Viewer::Known {
            login: "octocat".into(),
            avatar_url: "https://avatars.example/u/9".into(),
            profile_url: "https://github.example/octocat".into(),
        }
    }

    /// The one state with an identity behind it: a name, a face, and a link.
    #[test]
    fn a_known_viewer_names_itself_and_is_clickable() {
        let identity = chip_identity(Some(&known()));
        assert_eq!(identity.tooltip, "octocat on GitHub");
        assert_eq!(
            identity.profile_url.as_deref(),
            Some("https://github.example/octocat")
        );
        assert_eq!(
            avatar_url(Some(&known())).as_deref(),
            Some("https://avatars.example/u/9")
        );
    }

    /// Before the first answer the chip says so. Rendering "Not signed in"
    /// here would make every correctly configured app flash a wrong verdict on
    /// launch, which is worse than saying nothing.
    #[test]
    fn no_answer_yet_is_not_a_verdict() {
        let identity = chip_identity(None);
        assert_eq!(identity.tooltip, "Checking GitHub identity…");
        assert_eq!(identity.profile_url, None);
        assert_eq!(avatar_url(None), None);
    }

    /// Both fallbacks say which one they are — the configuration answer names
    /// the missing token, the failure carries GitHub's own sentence.
    #[test]
    fn each_fallback_says_which_one_it_is() {
        let unauth = chip_identity(Some(&Viewer::Unauthenticated));
        assert!(unauth.tooltip.contains("Not signed in"), "{unauth:?}");
        assert!(unauth.tooltip.contains("token"), "{unauth:?}");

        let unknown = chip_identity(Some(&Viewer::Unknown {
            error: "Bad credentials".into(),
        }));
        assert!(unknown.tooltip.contains("Bad credentials"), "{unknown:?}");
    }

    /// The asymmetry, both halves. A fallback is re-asked on every refresh so
    /// a token sealed after launch lands on the next event; a settled identity
    /// is re-asked coarsely, so an app open across a rotation does not go on
    /// showing the previous account forever.
    #[test]
    fn a_settled_identity_is_re_asked_coarsely_and_a_fallback_immediately() {
        assert!(should_ask_for_viewer(None, None));
        assert!(should_ask_for_viewer(
            Some(&Viewer::Unauthenticated),
            Some(Duration::from_secs(1))
        ));
        assert!(should_ask_for_viewer(
            Some(&Viewer::Unknown {
                error: "Bad credentials".into()
            }),
            Some(Duration::from_secs(1))
        ));

        assert!(!should_ask_for_viewer(
            Some(&known()),
            Some(Duration::from_secs(60))
        ));
        assert!(should_ask_for_viewer(
            Some(&known()),
            Some(VIEWER_RECHECK + Duration::from_secs(1))
        ));
        // Settled but never timed — ask, rather than never asking again.
        assert!(should_ask_for_viewer(Some(&known()), None));
    }

    /// The chip is inert in every state but `Known`. This is the property that
    /// replaces #987: no identity means no link, rather than a link to
    /// somebody else.
    #[test]
    fn every_fallback_is_inert_and_faceless() {
        for viewer in [
            None,
            Some(Viewer::Unauthenticated),
            Some(Viewer::Unknown {
                error: "GitHub is not answering".into(),
            }),
        ] {
            let identity = chip_identity(viewer.as_ref());
            assert_eq!(identity.profile_url, None, "{viewer:?}");
            assert_eq!(avatar_url(viewer.as_ref()), None, "{viewer:?}");
            assert!(!identity.tooltip.is_empty(), "{viewer:?}");
        }
    }
}
