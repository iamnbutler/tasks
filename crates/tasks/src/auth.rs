//! GitHub sign-in by the OAuth device flow (#1002).
//!
//! Minting a PAT by hand is the wrong first ask: the user chooses scopes they
//! cannot evaluate, and a wrong answer fails late — polling works, intake
//! works, and the first merge 403s. The device authorization grant (RFC 8628)
//! asks nothing: `tasks auth login` shows a code, the human enters it at
//! github.com/login/device, and the token lands in the sealed store under
//! `github-token`, where the poller, the broker and the leases already look.
//! There is no client secret anywhere in this flow — [`CLIENT_ID`] is public
//! by design, which is exactly why device flow fits a CLI and a GUI equally.
//!
//! The registration decisions this module assumes are recorded on #1002
//! (2026-08-20): an **OAuth App**, not a GitHub App, with **non-expiring user
//! tokens** ("Expire user access tokens" unchecked). [`poll_for_token`]
//! enforces the second half rather than trusting it: a token that arrives
//! with `expires_in` or `refresh_token` is refused
//! ([`AuthError::ExpiringToken`]) and nothing is stored, because sealing it
//! would plant a credential that dies eight hours later — mid-build, mid-lease
//! — with nothing here teaching the sealed store or the broker about refresh.
//! The setting is only observable from a completed grant, so the check lives
//! at the one place a completed grant passes through.
//!
//! This module speaks HTTP and returns values; it prints nothing. The CLI in
//! `main.rs` owns the conversation with the human, and a future settings pane
//! drives the same two calls — the issue's rule is that two implementations
//! of the `authorization_pending` / `slow_down` / expiry handling would be
//! the bug, and this module is the one implementation.

use std::time::Duration;

use serde::Deserialize;
use tokio::time::{Instant, sleep};

use crate::redact::Secret;

/// The OAuth app's client id — public by design (it ships in every request a
/// device flow makes, and RFC 8628 has no client secret), so a constant in
/// the binary is the honest home. Registered 2026-08-20; the registration
/// record is on #1002.
pub const CLIENT_ID: &str = "Ov23ctsgjjutbtDCBFjh";

/// `repo` is the classic-OAuth scope covering everything the pipeline
/// exercises — read issues and pull requests, write issues and comments,
/// merge, push. `workflow` is separate and real: since #1015 this repository
/// carries `.github/workflows`, and a push that touches a workflow file is
/// refused outright for a token without it — which would fail a Builder
/// branch at egress for editing CI, long after setup looked fine.
pub const SCOPE: &str = "repo workflow";

/// Where the flow talks to GitHub. Overridden only by tests (the
/// `TASKS_BROKER_ANTHROPIC_UPSTREAM` precedent), via `GITHUB_OAUTH_URL` —
/// note this is github.com, not api.github.com: the OAuth endpoints do not
/// live on the API host, so `GITHUB_API_URL` is deliberately not reused.
pub const DEFAULT_BASE: &str = "https://github.com";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("GitHub did not answer: {0}")]
    Http(#[from] reqwest::Error),

    /// The human declined at the verification page. Terminal — polling again
    /// would nag GitHub about a decision that was already made.
    #[error("the authorization was declined at the verification page; nothing was stored")]
    Denied,

    /// The device code's own lifetime ran out (GitHub grants ~15 minutes).
    /// Worded for any surface — the CLI's remedy is rerunning `tasks auth
    /// login` and the pane's is its sign-in button, so the sentence names the
    /// act rather than either surface's spelling of it.
    #[error("the sign-in code expired before it was entered — start again for a fresh one")]
    Expired,

    /// The one refusal that is about the app's settings rather than this run:
    /// the grant completed, and what came back expires. See the module doc.
    #[error(
        "GitHub returned an expiring token, so the OAuth app still has \
         \"Expire user access tokens\" checked. Uncheck it in the app's \
         settings (the #1002 decision: the sealed store holds one \
         non-expiring token and nothing refreshes it), then run \
         `tasks auth login` again. Nothing was stored."
    )]
    ExpiringToken,

    /// Anything else GitHub names: `device_flow_disabled`,
    /// `incorrect_client_credentials`, `unsupported_grant_type`, …. The
    /// error slug is GitHub's own vocabulary and is reported verbatim.
    #[error("GitHub refused: {error}{}", description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default())]
    Github {
        error: String,
        description: Option<String>,
    },

    /// A 200 that carries neither a token nor an error slug.
    #[error("GitHub's answer had neither a token nor an error in it")]
    Malformed,
}

/// What `POST /login/device/code` granted: the half to show the human
/// (`user_code`, `verification_uri`) and the half to poll with
/// (`device_code`, held as a [`Secret`] — it is not a credential yet, but
/// whoever holds it collects the token the moment the human approves).
pub struct DeviceAuthorization {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    interval: u64,
    device_code: Secret,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// The token poll's answer. Private and deliberately not `Debug`: a struct
/// that can hold the token must have no derived rendering for it to leak
/// through. `access_token` becomes a [`Secret`] the moment it is looked at.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    /// New poll cadence when `error` is `slow_down`. GitHub sends the number;
    /// RFC 8628's fallback (current + 5s) covers a server that does not.
    interval: Option<u64>,
    /// Present exactly when the OAuth app is configured for expiring tokens —
    /// the pair [`AuthError::ExpiringToken`] refuses on.
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// Ask GitHub for a device authorization: the code to show the human and the
/// handle to poll with.
pub async fn request_code(base: &str) -> Result<DeviceAuthorization, AuthError> {
    let response: DeviceCodeResponse = reqwest::Client::new()
        .post(format!("{base}/login/device/code"))
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(DeviceAuthorization {
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        expires_in: response.expires_in,
        interval: response.interval,
        device_code: Secret::new(response.device_code),
    })
}

/// Poll until the human authorizes, and return the token — or the refusal.
///
/// `authorization_pending` waits the current interval; `slow_down` adopts the
/// interval GitHub names (falling back to +5s, per RFC 8628); everything else
/// is terminal. The loop is additionally bounded by the device code's own
/// `expires_in` plus one interval of slack, so a GitHub that stops answering
/// definitively cannot hold the CLI forever — GitHub normally ends the flow
/// itself with `expired_token`, and the local bound reports the same
/// [`AuthError::Expired`] because it means the same thing. No
/// [`crate::deadline`] here: that machinery exists to bill run budgets fairly
/// on a host that sleeps, and a login is an interactive act where a laptop
/// that napped just finds `expired_token` on the next poll.
pub async fn poll_for_token(
    base: &str,
    authorization: &DeviceAuthorization,
) -> Result<Secret, AuthError> {
    let client = reqwest::Client::new();
    let mut interval = authorization.interval;
    let deadline = Instant::now()
        + Duration::from_secs(authorization.expires_in)
        + Duration::from_secs(interval);

    loop {
        sleep(Duration::from_secs(interval)).await;
        if Instant::now() > deadline {
            return Err(AuthError::Expired);
        }

        let response: TokenResponse = client
            .post(format!("{base}/login/oauth/access_token"))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", authorization.device_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        match response.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval = response.interval.unwrap_or(interval + 5);
                continue;
            }
            Some("access_denied") => return Err(AuthError::Denied),
            Some("expired_token") => return Err(AuthError::Expired),
            Some(other) => {
                return Err(AuthError::Github {
                    error: other.to_string(),
                    description: response.error_description,
                });
            }
            None => {
                // The refusal comes before the token is even wrapped: an
                // expiring grant must leave nothing behind to store by
                // accident.
                if response.expires_in.is_some() || response.refresh_token.is_some() {
                    return Err(AuthError::ExpiringToken);
                }
                let Some(token) = response.access_token else {
                    return Err(AuthError::Malformed);
                };
                return Ok(Secret::new(token));
            }
        }
    }
}

// --- the server-side flow, which is what the app drives (#1061) ------------

/// One GitHub sign-in at a time, held in memory by the server and driven over
/// two human-only routes: `POST /auth/github/device` starts a flow, and
/// `GET /auth/github/device` reports where it stands. The server polls GitHub
/// itself and seals the token directly, so **the token never transits the
/// app** — the pane only ever sees status, which is strictly better custody
/// than a paste field.
///
/// In-memory deliberately: a sign-in is an interactive act with a ~15-minute
/// code, and a server restart landing back on [`DeviceFlow::Idle`] costs the
/// human one more button press. A table for it would be a row asserting a
/// liveness nothing maintains.
///
/// One flow, not a map of them: the subject is "this server's GitHub
/// credential", of which there is exactly one. A second start while one is
/// pending supersedes it — the human pressing the button again is the human
/// deciding the old code is stale — and the superseded poll task is aborted
/// so it cannot write a dead flow's outcome over the live one's. The
/// `generation` guard is belt-and-braces beside the abort: an abort is
/// asynchronous, and a task that had already read its outcome must still find
/// out it lost the race at the one place state is written.
pub struct DeviceFlows {
    data_dir: std::path::PathBuf,
    secrets: crate::secrets::Secrets,
    base: String,
    inner: std::sync::Mutex<FlowInner>,
}

struct FlowInner {
    generation: u64,
    state: tasks_api::http::DeviceFlow,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl DeviceFlows {
    /// `base` is where the flow talks to GitHub — [`DEFAULT_BASE`] in
    /// production, the recording fake in tests.
    pub fn new(
        data_dir: std::path::PathBuf,
        secrets: crate::secrets::Secrets,
        base: String,
    ) -> Self {
        Self {
            data_dir,
            secrets,
            base,
            inner: std::sync::Mutex::new(FlowInner {
                generation: 0,
                state: tasks_api::http::DeviceFlow::Idle,
                task: None,
            }),
        }
    }

    /// Start (or supersede) the sign-in: ask GitHub for a code, hand it back
    /// for the pane to show, and poll in the background until the flow ends.
    pub async fn start(
        self: &std::sync::Arc<Self>,
    ) -> Result<tasks_api::http::DeviceFlowStatus, AuthError> {
        // The GitHub call happens before any state moves: a start that cannot
        // get a code leaves whatever was there — a sealed report, an earlier
        // refusal — rather than replacing it with a pending flow that can
        // never conclude.
        let authorization = request_code(&self.base).await?;
        let expires_at = chrono::Utc::now()
            + chrono::Duration::seconds(i64::try_from(authorization.expires_in).unwrap_or(900));

        let mut inner = self.inner.lock().expect("device flow lock poisoned");
        inner.generation += 1;
        let generation = inner.generation;
        if let Some(superseded) = inner.task.take() {
            superseded.abort();
        }
        inner.state = tasks_api::http::DeviceFlow::Pending {
            user_code: authorization.user_code.clone(),
            verification_uri: authorization.verification_uri.clone(),
            expires_at,
        };
        // Spawned under the lock (nothing here awaits), so a racing second
        // `start` cannot interleave between the state write and the task that
        // stands behind it.
        let this = std::sync::Arc::clone(self);
        inner.task = Some(tokio::spawn(async move {
            let outcome = match poll_for_token(&this.base, &authorization).await {
                // `expose` at the seal site only — the one conspicuous read,
                // same as the CLI's.
                Ok(token) => match crate::secrets::set(
                    &this.data_dir,
                    crate::secrets::SecretName::GithubToken,
                    token.expose(),
                ) {
                    Ok(()) => tasks_api::http::DeviceFlow::Sealed {
                        at: chrono::Utc::now(),
                    },
                    // The grant succeeded and the store write did not: a
                    // refusal, with the store's own sentence, because "sealed"
                    // here would claim a credential the broker will not find.
                    Err(error) => tasks_api::http::DeviceFlow::Refused {
                        message: format!("the token arrived but could not be sealed: {error}"),
                        at: chrono::Utc::now(),
                    },
                },
                Err(error) => tasks_api::http::DeviceFlow::Refused {
                    message: error.to_string(),
                    at: chrono::Utc::now(),
                },
            };
            let mut inner = this.inner.lock().expect("device flow lock poisoned");
            if inner.generation == generation {
                inner.state = outcome;
                inner.task = None;
            }
        }));
        drop(inner);

        Ok(self.status())
    }

    /// Where the sign-in stands, plus what currently serves as the GitHub
    /// credential — the observation the app could never make before this
    /// route existed (`empty_state.rs` hands that question here).
    pub fn status(&self) -> tasks_api::http::DeviceFlowStatus {
        tasks_api::http::DeviceFlowStatus {
            credential: self
                .secrets
                .source_of(crate::secrets::SecretName::GithubToken)
                .map(|source| source.to_string()),
            flow: self
                .inner
                .lock()
                .expect("device flow lock poisoned")
                .state
                .clone(),
        }
    }
}
