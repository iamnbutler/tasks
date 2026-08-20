//! Who the server's own GitHub credential is, cached in memory.
//!
//! `app-gpui` hardcoded one maintainer's avatar and profile link (#987), so
//! anyone else running the app saw a stranger's face in the chat composer and
//! a click took them to a stranger's GitHub. The fix is not a login flow: the
//! identity that matters here — whose branches get pushed, whose issues get
//! closed — is exactly the one the server's token names, so a login would
//! stand a *second* identity beside it.
//!
//! **No table, no migration.** A login can be renamed and an avatar
//! re-uploaded: this is a GitHub-owned fact, which this codebase queries at
//! read time rather than persisting. The cache is a `Mutex<Option<Entry>>`
//! modelled on [`crate::github_health`], and three of its rules are
//! load-bearing:
//!
//! 1. **The failure is cached too**, for a short window. The app refreshes on
//!    every SSE event, and an uncached failure would be a retry storm against
//!    GitHub driven by an open window.
//! 2. **`Unauthenticated` is never cached**, and costs no request at all.
//!    Caching it would leave a token sealed a minute later shadowed behind a
//!    stale "no token" for the whole TTL — and `tasks secrets set
//!    github-token` is documented to rotate a *running* server.
//! 3. **The lock is never held across the await.** Two racing callers cost one
//!    extra GitHub call; a held lock would let one caller's network timeout
//!    stall the route for everyone.
//!
//! What may appear in [`tasks_api::http::Viewer::Unknown`] is bounded by
//! [`describe_failure`], because that string is rendered in a tooltip and a
//! tooltip is output.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tasks_api::http::Viewer;

use crate::github::{GhError, GitHubClient};

/// How long a successful answer stands. Generous: a login and an avatar move
/// approximately never, and the point of caching at all is that the app asks
/// on every event.
const SUCCESS_TTL: Duration = Duration::from_secs(30 * 60);

/// How long a failure stands. Short, because the fix — sealing a working token
/// — is expected to be applied to a *running* server, and a stale failure that
/// outlives it makes the fix look like it did not work.
const FAILURE_TTL: Duration = Duration::from_secs(5 * 60);

/// One remembered answer and when it was obtained.
#[derive(Debug, Clone)]
struct Entry {
    viewer: Viewer,
    at: DateTime<Utc>,
}

impl Entry {
    fn ttl(&self) -> Duration {
        match self.viewer {
            Viewer::Known { .. } => SUCCESS_TTL,
            // Never stored — see the module doc — but stated here rather than
            // left to the caller, so the policy is in one place.
            Viewer::Unauthenticated => Duration::ZERO,
            Viewer::Unknown { .. } => FAILURE_TTL,
        }
    }

    fn fresh_at(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.at);
        // A negative age is a clock that stepped backwards; treat it as fresh
        // rather than as infinitely stale.
        age < chrono::Duration::from_std(self.ttl()).unwrap_or_else(|_| chrono::Duration::zero())
            && age >= chrono::Duration::zero()
    }
}

/// The server's answer to `GET /viewer`, remembered for a while.
#[derive(Debug, Default)]
pub struct ViewerCache {
    entry: Mutex<Option<Entry>>,
}

impl ViewerCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Who the credential is, asking GitHub only when nothing fresh is held.
    ///
    /// Always answers — there is no error path. All three states are answers,
    /// and a router with no GitHub client answers `Unauthenticated` without a
    /// request, which is also the honest answer for a server nobody has sealed
    /// a token into.
    pub async fn get(&self, github: Option<&Arc<GitHubClient>>) -> Viewer {
        let Some(github) = github else {
            return Viewer::Unauthenticated;
        };
        let now = Utc::now();
        if let Some(entry) = self.peek(now) {
            return entry;
        }
        // Deliberately outside the lock: see the module doc.
        let viewer = match github.viewer().await {
            Ok(v) => Viewer::Known {
                login: v.login,
                avatar_url: v.avatar_url,
                profile_url: v.profile_url,
            },
            Err(e) => Viewer::Unknown {
                error: describe_failure(&e),
            },
        };
        self.store(viewer.clone(), now);
        viewer
    }

    /// The held answer, if one is still fresh.
    fn peek(&self, now: DateTime<Utc>) -> Option<Viewer> {
        let held = self.entry.lock().ok()?;
        held.as_ref()
            .filter(|e| e.fresh_at(now))
            .map(|e| e.viewer.clone())
    }

    fn store(&self, viewer: Viewer, at: DateTime<Utc>) {
        // `Unauthenticated` is never reached here today (no client means no
        // request), but the policy is enforced rather than assumed: a later
        // caller that can produce one must not be able to shadow a token
        // sealed a minute afterwards.
        if matches!(viewer, Viewer::Unauthenticated) {
            return;
        }
        if let Ok(mut held) = self.entry.lock() {
            *held = Some(Entry { viewer, at });
        }
    }
}

/// What a failed lookup is allowed to say.
///
/// The string reaches a tooltip in the app, and a tooltip is output: #971's
/// rule that no credential or fragment of one appears in output applies here.
/// So this carries **GitHub's own response message and nothing derived from
/// the request** — no URL, no headers, and in particular not a
/// [`reqwest::Error`]'s `Display`, which quotes the URL it was sent to.
///
/// The two remaining arms are fixed sentences rather than payloads: a
/// transport failure has nothing to say that is not about our request, and a
/// shape complaint is about our own parsing and belongs in the log the caller
/// already has.
pub fn describe_failure(err: &GhError) -> String {
    match err {
        // GitHub's own words, from the `errors` block and from a REST body's
        // `message` — "Bad credentials" is exactly the sentence an operator
        // needs, and it is the reason `viewer()` reads `errors` first.
        GhError::GraphQl(message) => message.clone(),
        GhError::Rest { message, .. } => message.clone(),
        GhError::Http(_) => "GitHub is not answering".to_string(),
        GhError::Shape(_) => "GitHub's answer carried no identity".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(login: &str) -> Viewer {
        Viewer::Known {
            login: login.into(),
            avatar_url: "https://avatars.example/u/1".into(),
            profile_url: "https://github.example/u".into(),
        }
    }

    /// A held success is what the next caller gets, without a GitHub client
    /// being consulted at all.
    #[test]
    fn a_fresh_success_is_reused() {
        let cache = ViewerCache::new();
        let now = Utc::now();
        cache.store(known("octocat"), now);
        assert_eq!(cache.peek(now), Some(known("octocat")));
        assert_eq!(
            cache.peek(now + chrono::Duration::minutes(29)),
            Some(known("octocat"))
        );
        assert_eq!(cache.peek(now + chrono::Duration::minutes(31)), None);
    }

    /// The failure is cached — that is the "no retry storm" — but for minutes
    /// rather than half an hour, because sealing a working token is expected
    /// to fix it live.
    #[test]
    fn a_failure_is_cached_briefly() {
        let cache = ViewerCache::new();
        let now = Utc::now();
        let failed = Viewer::Unknown {
            error: "Bad credentials".into(),
        };
        cache.store(failed.clone(), now);
        assert_eq!(cache.peek(now + chrono::Duration::minutes(4)), Some(failed));
        assert_eq!(cache.peek(now + chrono::Duration::minutes(6)), None);
    }

    /// The rule that makes `tasks secrets set github-token` work on a running
    /// server: "no credential" is never remembered, so the very next read
    /// asks again.
    #[test]
    fn unauthenticated_is_never_cached() {
        let cache = ViewerCache::new();
        let now = Utc::now();
        cache.store(Viewer::Unauthenticated, now);
        assert_eq!(cache.peek(now), None);
    }

    /// The bound on what reaches a tooltip: GitHub's sentence when GitHub
    /// wrote one, and a fixed sentence otherwise — never the request.
    #[test]
    fn a_failure_reports_githubs_words_and_never_the_request() {
        assert_eq!(
            describe_failure(&GhError::GraphQl("Bad credentials".into())),
            "Bad credentials"
        );
        assert_eq!(
            describe_failure(&GhError::Rest {
                what: "viewer".into(),
                status: reqwest::StatusCode::UNAUTHORIZED,
                message: "Bad credentials".into(),
            }),
            "Bad credentials"
        );
        let shape = describe_failure(&GhError::Shape(
            "viewer.login, viewer.avatarUrl or viewer.url is absent".into(),
        ));
        assert_eq!(shape, "GitHub's answer carried no identity");
        assert!(!shape.contains("viewer.login"));
    }

    /// No client means no request and no cache write, so the answer cannot go
    /// stale over a token sealed after it.
    #[tokio::test]
    async fn no_credential_answers_without_asking() {
        let cache = ViewerCache::new();
        assert_eq!(cache.get(None).await, Viewer::Unauthenticated);
        assert_eq!(cache.peek(Utc::now()), None);
    }
}
