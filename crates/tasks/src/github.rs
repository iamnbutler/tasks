//! GitHub GraphQL client and issue normalization.
//!
//! The client fetches issues from a single repository via GraphQL and
//! returns normalized [`GhIssue`] records. Persistence + task upsert lives
//! on [`crate::store::Store`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use crate::models::{CloseReason, GhState};

const DEFAULT_BASE_URL: &str = "https://api.github.com/graphql";
const DEFAULT_REST_BASE_URL: &str = "https://api.github.com";
const PAGE_SIZE: u32 = 100;
/// Labels requested per issue. GitHub caps an issue at 100 labels, so this is
/// "all of them" — and it has to be, because [`IntakeFilter`] reads a truncated
/// label list as "the intake label is absent" and drops the issue from intake
/// with nothing to say why.
const LABEL_PAGE_SIZE: u32 = 100;
const CLOSE_INFO_BATCH: usize = 50;

#[derive(Debug, Error)]
pub enum GhError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("graphql: {0}")]
    GraphQl(String),
    /// A REST call GitHub answered with a non-2xx.
    ///
    /// A struct variant carrying the status as a [`reqwest::StatusCode`] rather
    /// than one pre-rendered `String`, because [`GhError::is_unavailable`]
    /// decides on it — and a decision that greps prose changes meaning the next
    /// time somebody improves a sentence, which is the same rule
    /// `FailureClass` follows. The rendered text is deliberately unchanged:
    /// this message is read by humans in warnings and in failure reasons, so
    /// only the shape moved.
    #[error("rest: {what}: {status}: {message}")]
    Rest {
        what: String,
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("unexpected response shape: {0}")]
    Shape(String),
}

impl GhError {
    /// Whether this failure means **GitHub is not answering** — as opposed to
    /// answering something we did not like.
    ///
    /// Read by [`crate::github_health::GitHubHealth`], which holds scout and
    /// build dispatch while it is true: a Scout clones and a Builder clones, so
    /// work dispatched into an outage dies at its first step and is charged a
    /// strike for it (#939).
    ///
    /// Four exclusions are deliberate, and each would make the hold either
    /// wrong or permanent:
    ///
    /// - **429** is a fact about our own usage, it names its own reset, and a
    ///   clone does not spend the API quota.
    /// - Any other **4xx** is GitHub answering perfectly well; a 404 on one
    ///   pull request says nothing about the service.
    /// - **Decode failures** (`Shape`, a body that would not parse) are our
    ///   bug, not an outage.
    /// - **Builder errors** — a malformed request that never left the process
    ///   — are permanent misconfiguration. A hold on any of these would clear
    ///   from nowhere.
    pub fn is_unavailable(&self) -> bool {
        match self {
            // A response with a status: only 5xx. Without one, the request
            // never got an answer at all — connect refused, timed out, or died
            // in flight.
            Self::Http(e) => match e.status() {
                Some(status) => status.is_server_error(),
                None => e.is_connect() || e.is_timeout() || e.is_request(),
            },
            Self::Rest { status, .. } => status.is_server_error(),
            Self::GraphQl(_) | Self::Shape(_) => false,
        }
    }
}

/// Build a [`GhError::Rest`] from a status and GitHub's own response body.
///
/// The one place the variant is constructed. GitHub's failure messages are the
/// useful half of a failed write — "Pull Request is not mergeable", "Resource
/// not accessible by integration" — and dropping them for the bare status is
/// what makes a permissions problem look identical to a conflict.
fn rest_error(
    what: impl Into<String>,
    status: reqwest::StatusCode,
    body: &serde_json::Value,
) -> GhError {
    GhError::Rest {
        what: what.into(),
        status,
        message: body
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(no message)")
            .to_string(),
    }
}

/// Unwrap a REST response, turning a non-2xx into a [`GhError::Rest`] that
/// carries GitHub's own `message`.
async fn rest_ok(resp: reqwest::Response, what: &str) -> Result<serde_json::Value, GhError> {
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(rest_error(what, status, &body));
    }
    Ok(body)
}

/// What an issue looks like right now, for a reconciliation reading back an
/// effect it never saw the answer to. Never persisted — every field here is
/// GitHub's and is read at decision time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueFacts {
    pub state: GhState,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// Normalized GitHub issue, ready for upsert into tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhIssue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: GhState,
    pub updated_at: DateTime<Utc>,
}

/// Which fetched issues are allowed into intake (`TASKS_INTAKE_LABEL`).
///
/// Applied by [`crate::run::poll_once`] *after* the fetch, never as a `labels:`
/// argument on the GraphQL query: [`crate::store::Store::reconcile_closed_issues`]
/// infers upstream closure from absence from the open set, so it must keep
/// receiving the *complete* open set. Filtering in the query would make every
/// task whose issue merely lost the label look closed. The cost is that the
/// poller still pages through every open issue — deliberate; this is not an
/// API-cost optimization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IntakeFilter {
    /// Every open issue is ingested. The default, and what every deployment
    /// that never sets `TASKS_INTAKE_LABEL` gets.
    #[default]
    All,
    /// Only issues carrying this label are ingested.
    Label(String),
}

impl IntakeFilter {
    /// Resolve the configured label. Absent, empty, or whitespace-only all read
    /// as [`IntakeFilter::All`] — a bare `TASKS_INTAKE_LABEL=` in a `.env` means
    /// "unset", not "a label no issue can carry", which would silently halt all
    /// intake.
    pub fn from_label(label: Option<String>) -> Self {
        match label {
            Some(label) if !label.trim().is_empty() => Self::Label(label.trim().to_string()),
            _ => Self::All,
        }
    }

    /// Whether this issue may be ingested. Matching is ASCII-case-insensitive:
    /// GitHub refuses to create two labels differing only in case, so a
    /// case-sensitive match would just be a footgun (`Tasks` vs `tasks`).
    pub fn admits(&self, issue: &GhIssue) -> bool {
        match self {
            Self::All => true,
            Self::Label(want) => issue
                .labels
                .iter()
                .any(|have| have.eq_ignore_ascii_case(want)),
        }
    }

    /// The configured label, for startup logging.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Label(label) => Some(label),
        }
    }
}

/// How a specific issue looks right now — state plus GitHub's close reason
/// (`stateReason`: COMPLETED, NOT_PLANNED, DUPLICATE, ...). Fetched at
/// decision time and never persisted; GitHub owns these facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCloseInfo {
    pub state: GhState,
    pub state_reason: Option<String>,
}

/// How a pull request looks right now, queried live and never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrState {
    pub state: GhState,
    /// **"Reached its base", not "shipped."** A stacked PR based on another
    /// build's branch reads `merged: true` the moment that branch takes it,
    /// and the branch itself may never reach the trunk — which is exactly how
    /// PR #863 was lost. Anything deciding delivery has to ask whether the
    /// merge commit is an ancestor of the trunk; see
    /// [`GitHubClient::merge_reached_trunk`] and `run::shipped`.
    pub merged: bool,
    /// GitHub's mergeability verdict, or `None` while it is still computing
    /// one. Unknown is not the same as conflicted.
    ///
    /// **Too coarse to act on**: `false` means a conflict and nothing else, so
    /// a pull request behind a failing required check reads `true` here. Use
    /// [`PrState::landing`], which prefers `mergeable_state`.
    pub mergeable: Option<bool>,
    /// GitHub's finer verdict — `clean`, `dirty`, `blocked`, `behind`,
    /// `unstable`, `draft`, `unknown` — off the same body. `None` when the
    /// field is absent, which is not the same as `Some("unknown")` but is
    /// treated identically: neither is a block.
    pub mergeable_state: Option<String>,
    /// The commit the merge produced — evidence for closing the issue the PR
    /// implements.
    ///
    /// **Populated on open PRs too**, from GitHub's speculative test merge, so
    /// its presence says nothing about whether anything landed. Check `merged`
    /// first, always.
    pub merge_commit_sha: Option<String>,
    /// The branch this PR merges *into* — `base.ref`. The cheap half of the
    /// shipped question: a PR based on the trunk needs no further reads.
    pub base_ref: Option<String>,
    /// The branch this PR merges *from* — `head.ref`. What another build
    /// stacks **on**, and therefore what a squash would make unreachable.
    pub head_ref: Option<String>,
}

impl PrState {
    /// One word for a brief line: what a reader needs to know about this PR.
    pub fn label(&self) -> &'static str {
        match (self.state, self.merged, self.mergeable) {
            (_, true, _) => "merged",
            (GhState::Closed, false, _) => "closed unmerged",
            (GhState::Open, false, Some(false)) => "open, conflicts",
            (GhState::Open, false, _) => "open",
        }
    }

    /// Whether GitHub would take this merge right now, read at the resolution
    /// that can actually object.
    ///
    /// `mergeable_state` is consulted **before** `mergeable`, and that ordering
    /// is the whole point: `mergeable` is `false` only for a conflict, so a
    /// pull request sitting behind a failing *required* check answers `true`
    /// and reads ready. Only `blocked` says otherwise.
    ///
    /// `merged` and closed-unmerged short-circuit first, because GitHub keeps
    /// answering `mergeable_state` on a closed pull request and the answer is
    /// about a merge that can no longer happen. An unrecognized or absent
    /// state falls back to the coarse flag, and an absent flag is
    /// [`Landing::Unknown`] — not a block, and not a clearance either.
    pub fn landing(&self) -> Landing {
        if self.merged {
            return Landing::Merged;
        }
        if self.state == GhState::Closed {
            return Landing::ClosedUnmerged;
        }
        match self.mergeable_state.as_deref() {
            Some("clean") => Landing::Clear,
            Some("unstable") => Landing::Unstable,
            Some("dirty") => Landing::Blocked(BLOCKED_CONFLICT),
            Some("blocked") => Landing::Blocked(BLOCKED_REQUIRED),
            Some("draft") => Landing::Blocked(BLOCKED_DRAFT),
            Some("behind") => Landing::Blocked(BLOCKED_BEHIND),
            // "unknown", anything GitHub adds later, or no field at all: the
            // coarse flag is all there is, and it is allowed to be absent.
            _ => match self.mergeable {
                Some(true) => Landing::Clear,
                Some(false) => Landing::Blocked(BLOCKED_CONFLICT),
                None => Landing::Unknown,
            },
        }
    }
}

/// The reasons a [`Landing::Blocked`] carries. A fixed set of `&'static str`
/// rather than GitHub's own prose, so tests can assert on them and a caller
/// cannot end up rendering a message nobody wrote — a future case that needs
/// GitHub's words wants its own variant, not a `String` here.
const BLOCKED_CONFLICT: &str = "the branch conflicts with its base";
const BLOCKED_REQUIRED: &str = "a required review or status check has not passed";
const BLOCKED_DRAFT: &str = "the pull request is still a draft";
const BLOCKED_BEHIND: &str =
    "the branch is behind its base and the repository requires it to be current";

/// Whether a pull request can be landed *as a merge* — never whether the
/// change is any good.
///
/// The distinction is load-bearing and #1015 moved where it falls.
/// `.github/workflows/ci.yml` now runs `cargo fmt`, clippy and the whole suite
/// on **every push**, and a Builder branch is pushed into this repository, so
/// its check runs attach to the head commit a pull request points at. Three
/// consequences, and none of them is the obvious one.
///
/// [`Self::Clear`] now carries evidence: `clean` requires every check GitHub
/// knows about to have *passed*, so on this repository it says the branch
/// builds, is formatted, is clippy-clean and its tests pass. What it still does
/// not say is that the change works composed with a trunk that has moved since
/// — no run against a branch can, and that is what the merge brief's own
/// carve-outs are about.
///
/// [`Self::Unstable`] becomes reachable, and it is the dangerous one, because
/// GitHub does not distinguish "a check failed" from "a check has not finished
/// yet" — both read `unstable`. Merging on it without finding out which is the
/// new way to ship a red branch, so [`Landing::describe`] says so rather than
/// dismissing it as non-required noise.
///
/// [`Self::Blocked`]`(BLOCKED_REQUIRED)` stays unreachable here: the checks are
/// **not required**, because there is no branch protection on this repository,
/// so GitHub will still take a merge over a red one. The arm stays for the day
/// that changes, which is a separate decision about who may land.
///
/// The premise is not left to prose. `ci_runs_the_suite_on_every_push` in
/// `crates/tasks/tests/site.rs` fails the suite if CI stops existing or grows a
/// `branches:`/`paths:` filter — the quiet failure, since an unchecked branch
/// reads `clean` because nothing ran rather than because something passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// Nothing in the way of the merge itself.
    Clear,
    /// Mergeable, with a non-required check failing or still running.
    Unstable,
    /// GitHub would refuse the merge, for the named reason.
    Blocked(&'static str),
    /// GitHub has not computed a verdict yet. Not a block.
    Unknown,
    /// Already merged into its base — which is not the same as shipped.
    Merged,
    /// Closed without merging.
    ClosedUnmerged,
}

impl Landing {
    /// One sentence for a brief line.
    ///
    /// [`Self::Clear`] deliberately says what it does *not* mean. A reader —
    /// human or agent — who takes "clean" for "verified" is reading a signal
    /// that, in a repository with no required checks, is structurally
    /// incapable of objecting to a change that does not work.
    pub fn describe(&self) -> String {
        match self {
            Landing::Clear => "GitHub reports nothing in the way of the merge itself — no \
                 conflict with the base, and every check it knows about has passed, \
                 which here is fmt, clippy and the whole suite against this branch. \
                 That says nothing about whether the branch still passes composed \
                 with a trunk that has moved since its base."
                .to_string(),
            Landing::Unstable => "GitHub would take the merge, and a check on the head commit is \
                 either FAILING or has not finished — GitHub reports both as the \
                 same state and does not say which. Here a check is this project's \
                 own fmt, clippy and test suite, so find out which one it is before \
                 merging; nothing will refuse the merge for you."
                .to_string(),
            Landing::Blocked(reason) => format!(
                "GitHub would refuse this merge: {reason}. A merge call now would \
                 be refused, so that has to be cleared first."
            ),
            Landing::Unknown => "GitHub has not computed a mergeability verdict yet (it does that \
                 lazily, usually within seconds of a push). That is not a block — \
                 the next reminder asks again."
                .to_string(),
            Landing::Merged => {
                "the pull request is already merged into its base, which is not the \
                 same as having reached the trunk."
                    .to_string()
            }
            Landing::ClosedUnmerged => {
                "the pull request was closed without merging, so there is no merge left to make."
                    .to_string()
            }
        }
    }
}

/// Where the client's bearer token comes from at request time.
///
/// `Live` reads **through** [`crate::secrets::Secrets`] on every request, so
/// `tasks secrets set github-token` rotates a running server's API calls
/// with nothing restarted — including the `Arc<GitHubClient>`s the server
/// and the Builder hold for their whole lifetime. `Fixed` is the test path
/// and any caller that already resolved a token.
enum TokenSource {
    Fixed(crate::redact::Secret),
    Live(crate::secrets::Secrets),
}

/// Who a token is and what it may do, as [`GitHubClient::viewer`] found out.
///
/// Named `GhViewer` and not `Viewer` on purpose: [`tasks_api::http::Viewer`]
/// is the *wire* answer a client reads, with three states and no scopes, and
/// two types with one name meaning different things across that boundary is a
/// trap. This one is GitHub's answer; that one is ours.
///
/// Two readers share it — `tasks doctor`, which wants the login and the
/// scopes, and `GET /viewer`, which wants the login, the avatar and the
/// profile link — because it is **one GraphQL round trip either way**, and two
/// methods asking GitHub the same question is strictly worse than one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GhViewer {
    /// The account or app the token authenticates as.
    pub(crate) login: String,
    /// Where this account's avatar image lives. Rendered by the app's chat
    /// chip; nothing here fetches it.
    pub(crate) avatar_url: String,
    /// **GitHub's own `url`**, never `https://github.com/{login}` assembled
    /// from the login — on GitHub Enterprise the origin is not github.com and
    /// a link built that way opens the wrong host.
    pub(crate) profile_url: String,
    /// Classic-PAT scopes, or `None` where **no response carried the header
    /// at all** — fine-grained PATs and GitHub App tokens have permissions
    /// rather than scopes and send none.
    ///
    /// `None` and `Some(vec![])` must stay distinguishable the whole way to
    /// the renderer: they are opposite verdicts. "This token type does not
    /// enumerate its permissions here" is fine; "this token has no scopes at
    /// all" means replace it, and telling an operator to replace a token that
    /// works is the failure this distinction exists to prevent.
    pub(crate) scopes: Option<Vec<String>>,
    /// Which response the scopes came off, so a reader can see *that* the
    /// premise held rather than inferring it. `None` exactly when `scopes` is.
    pub(crate) scope_source: Option<ScopeSource>,
}

/// Which response carried `x-oauth-scopes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeSource {
    /// The GraphQL response itself — one round trip, no fallback needed.
    GraphQlHeader,
    /// `GET /rate_limit`, the documented source.
    RestHeader,
}

impl std::fmt::Display for ScopeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GraphQlHeader => f.write_str("graphql response header"),
            Self::RestHeader => f.write_str("REST /rate_limit response header"),
        }
    }
}

/// `x-oauth-scopes` off a response, as a list.
///
/// Absent is `None` and present-but-empty is `Some(vec![])` — see
/// [`GhViewer::scopes`]. GitHub sends the list comma-separated and normalizes
/// it on issue (`user,gist,user:email` is stored as `user, gist`), so this
/// splits and trims and does not attempt to compare against anything.
fn scopes_from(headers: &reqwest::header::HeaderMap) -> Option<Vec<String>> {
    let raw = headers.get("x-oauth-scopes")?.to_str().ok()?;
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

pub struct GitHubClient {
    http: reqwest::Client,
    token: TokenSource,
    base_url: String,
    rest_base_url: String,
}

impl GitHubClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::with_source(
            TokenSource::Fixed(crate::redact::Secret::new(token.into())),
            base_url,
        )
    }

    /// A client whose token is read live from the sealed store (with its env
    /// fallback) at every request. `api_url` overrides the GraphQL endpoint.
    pub fn from_secrets(secrets: crate::secrets::Secrets, api_url: Option<&str>) -> Self {
        Self::with_source(
            TokenSource::Live(secrets),
            api_url
                .map(str::to_string)
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        )
    }

    fn with_source(token: TokenSource, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("tasks/0.1")
            .build()
            .expect("reqwest client");
        Self {
            http,
            token,
            base_url: base_url.into(),
            rest_base_url: DEFAULT_REST_BASE_URL.into(),
        }
    }

    /// The bearer token as of *now*. An empty value (a live source whose
    /// token was removed mid-flight) authenticates as nothing and fails
    /// upstream exactly as an expired token would — the caller's error
    /// handling is already shaped for that.
    fn token(&self) -> crate::redact::Secret {
        match &self.token {
            TokenSource::Fixed(secret) => secret.clone(),
            TokenSource::Live(secrets) => secrets
                .github_token()
                .unwrap_or_else(|| crate::redact::Secret::new("")),
        }
    }

    /// Override the REST root (`GITHUB_REST_API_URL`) — GitHub Enterprise,
    /// tests. Issues stay on GraphQL; only PR creation uses REST.
    pub fn with_rest_base_url(mut self, rest_base_url: impl Into<String>) -> Self {
        self.rest_base_url = rest_base_url.into();
        self
    }

    /// Open a pull request. Returns the PR *number*: an immutable identifier
    /// we may persist. The PR's state/mergeability/checks are GitHub's and are
    /// not read here, let alone stored.
    ///
    /// Every GitHub write in this file lives on the server for the same
    /// reason — an agent writing through its own credential leaves no ledger
    /// row, no event, and nothing the charter can reach.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        name: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<u64, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls", self.rest_base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "title": title,
                "head": head,
                "base": base,
                "body": body,
            }))
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(rest_error("create pull request", status, &body));
        }
        body.get("number")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| GhError::Shape("pull request response missing `number`".into()))
    }

    /// File an issue. Returns its number — the identifier we may persist;
    /// everything else about the issue stays GitHub's and is read back by the
    /// poller like any other issue.
    pub async fn create_issue(
        &self,
        owner: &str,
        name: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<u64, GhError> {
        let url = format!("{}/repos/{owner}/{name}/issues", self.rest_base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "title": title,
                "body": body,
                "labels": labels,
            }))
            .send()
            .await?;
        // Through `rest_ok`, like every other write here — and it was the one
        // that was not. Parsing the body *before* checking the status turns a
        // 5xx whose body is not JSON (a proxy's error page) into
        // `GhError::Http(decode error)`, which carries no status, so
        // `is_unavailable()` reads it as GitHub *answering* — and the intent
        // ledger would annul a decision whose issue may well have been filed.
        let body = rest_ok(resp, "create issue").await?;
        body.get("number")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| GhError::Shape("issue response missing `number`".into()))
    }

    /// Close an issue, stating why.
    ///
    /// The write happens here; the *fact* that the issue is closed is still
    /// learned from the poller reading GitHub's open set, exactly as for an
    /// issue a human closed in the browser. Nothing is marked closed locally
    /// in anticipation — write path and read path stay separate.
    pub async fn close_issue(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        reason: CloseReason,
    ) -> Result<(), GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}",
            self.rest_base_url
        );
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "state": "closed",
                "state_reason": reason.as_str(),
            }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            return Err(rest_error(format!("close issue {number}"), status, &body));
        }
        Ok(())
    }

    /// Reopen a closed issue.
    ///
    /// The inverse of [`GitHubClient::close_issue`], and the reason closing can
    /// be trusted with autonomy at all: a wrong retirement costs a reopen, not
    /// an apology. Same read/write split — the poller learns the issue is open
    /// again on its next pass, nothing is marked locally in anticipation.
    pub async fn reopen_issue(&self, owner: &str, name: &str, number: u64) -> Result<(), GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}",
            self.rest_base_url
        );
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "state": "open" }))
            .send()
            .await?;
        rest_ok(resp, &format!("reopen issue {number}")).await?;
        Ok(())
    }

    /// Comment on an issue or a pull request. Returns the comment id.
    ///
    /// One method for both: GitHub's `/issues/{n}/comments` accepts a PR
    /// number, because a PR is an issue with a branch attached and they share
    /// one number space. A separate `pull_request_comment` would be the same
    /// HTTP call with a misleading name.
    ///
    /// Note this is the *conversation* comment, not a review comment pinned to
    /// a diff line — that is a different resource, and worth adding only when
    /// something here actually wants to annotate a hunk.
    pub async fn create_issue_comment(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        body: &str,
    ) -> Result<u64, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}/comments",
            self.rest_base_url
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;
        let body = rest_ok(resp, &format!("comment on {number}")).await?;
        body.get("id")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| GhError::Shape("comment response missing `id`".into()))
    }

    /// The SHA at the tip of a pull request's branch, right now.
    pub async fn pull_request_head_sha(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<String, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls/{number}", self.rest_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, &format!("read pull request {number}")).await?;
        body.get("head")
            .and_then(|h| h.get("sha"))
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| GhError::Shape("pull request response missing `head.sha`".into()))
    }

    /// Comment on a specific line of a pull request's diff.
    ///
    /// Different resource from [`GitHubClient::create_issue_comment`], and
    /// different in kind: a thread comment is about the PR, this is about a
    /// line, and a review that names a file and a line survives where "the
    /// CARGO=/nonexistent-cargo test was dropped" in a chat log does not.
    ///
    /// The head SHA is read here rather than taken from the caller. GitHub
    /// anchors the comment to a commit, and a SHA that arrived through a
    /// prompt is exactly the sort of GitHub-owned fact this codebase refuses
    /// to carry around: by the time it is used the branch may have moved.
    ///
    /// `line` is the line number in the file *after* the change, and the file
    /// must appear in the diff. GitHub returns 422 otherwise, which is the
    /// right outcome — a review comment on an unchanged line is one nobody
    /// sees in the review UI.
    pub async fn create_review_comment(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        path: &str,
        line: u64,
        body: &str,
    ) -> Result<u64, GhError> {
        let commit_sha = self.pull_request_head_sha(owner, name, number).await?;
        let url = format!(
            "{}/repos/{owner}/{name}/pulls/{number}/comments",
            self.rest_base_url
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "body": body,
                "commit_id": commit_sha,
                "path": path,
                "line": line,
                "side": "RIGHT",
            }))
            .send()
            .await?;
        let body = rest_ok(resp, &format!("review comment on {number} {path}:{line}")).await?;
        body.get("id")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| GhError::Shape("review comment response missing `id`".into()))
    }

    /// Read an issue's current title and body.
    ///
    /// Exists for the edit path: rewriting a body without first reading it is
    /// how a correction becomes a deletion. The caller keeps the old text for
    /// the ledger.
    pub async fn issue_body(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<(String, String), GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}",
            self.rest_base_url
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, &format!("read issue {number}")).await?;
        Ok((
            body.get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            body.get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default()
                .to_string(),
        ))
    }

    /// Rewrite an issue's title and/or body.
    ///
    /// The one write here that destroys rather than appends. An issue filed on
    /// a theory that later turns out wrong is worse than no issue — the next
    /// reader inherits the superseded reasoning as if it still held — so this
    /// has to exist. What keeps it honest is upstream: the caller records the
    /// previous text on the decision, because the thing worth auditing is the
    /// diff, not the fact that an edit occurred.
    pub async fn update_issue(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<(), GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}",
            self.rest_base_url
        );
        let mut payload = serde_json::Map::new();
        if let Some(title) = title {
            payload.insert("title".into(), serde_json::Value::String(title.into()));
        }
        if let Some(body) = body {
            payload.insert("body".into(), serde_json::Value::String(body.into()));
        }
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::Value::Object(payload))
            .send()
            .await?;
        rest_ok(resp, &format!("edit issue {number}")).await?;
        Ok(())
    }

    /// Replace an issue's labels.
    ///
    /// PUT, not POST: the complete set, so removing a label is expressible.
    /// Pair it with [`GitHubClient::list_labels`] — labelling from a guessed
    /// vocabulary creates near-duplicates (`bug` vs `bugs`) that quietly
    /// fragment every future filter.
    pub async fn set_issue_labels(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        labels: &[String],
    ) -> Result<(), GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}/labels",
            self.rest_base_url
        );
        let resp = self
            .http
            .put(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "labels": labels }))
            .send()
            .await?;
        rest_ok(resp, &format!("set labels on {number}")).await?;
        Ok(())
    }

    /// The repository's label vocabulary: name and description.
    ///
    /// A read, but it belongs next to the writes it serves. Without it the
    /// only honest thing an agent can do is file with no labels at all, which
    /// is what has been happening.
    pub async fn list_labels(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Vec<(String, String)>, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/labels?per_page=100",
            self.rest_base_url
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, "list labels").await?;
        let arr = body
            .as_array()
            .ok_or_else(|| GhError::Shape("labels response is not an array".into()))?;
        Ok(arr
            .iter()
            .filter_map(|l| {
                Some((
                    l.get("name")?.as_str()?.to_string(),
                    l.get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                ))
            })
            .collect())
    }

    /// Everything about an issue that a reconciliation reads back: its state,
    /// its current text, and its labels.
    ///
    /// One GET serving four actions (`retire_work`, `reopen_work`,
    /// `edit_issue`, `label_issue`), because they all ask the same resource a
    /// different question and four methods would be four chances for two of
    /// them to disagree about what a 404 means.
    pub async fn issue_facts(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<IssueFacts, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/issues/{number}",
            self.rest_base_url
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, &format!("read issue {number}")).await?;
        Ok(IssueFacts {
            state: match body.get("state").and_then(|s| s.as_str()) {
                Some("closed") => GhState::Closed,
                _ => GhState::Open,
            },
            title: body
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            body: body
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default()
                .to_string(),
            labels: body
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|l| Some(l.get("name")?.as_str()?.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Issue numbers in this repository whose title matches `title` exactly,
    /// open or closed.
    ///
    /// The reconciliation for a `capture_work` whose call never answered: the
    /// only handle we have on an issue we may or may not have filed is the
    /// title we sent. Search is eventually consistent, so a just-filed issue
    /// may not appear — which is fine here and only here, because this is read
    /// after a grace period rather than in the moment.
    pub async fn find_issues_by_title(
        &self,
        owner: &str,
        name: &str,
        title: &str,
    ) -> Result<Vec<u64>, GhError> {
        let query = format!("repo:{owner}/{name} in:title type:issue \"{title}\"");
        let url = format!("{}/search/issues", self.rest_base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("q", query.as_str()), ("per_page", "20")])
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, "search issues by title").await?;
        let items = body
            .get("items")
            .and_then(|i| i.as_array())
            .ok_or_else(|| GhError::Shape("search response missing `items`".into()))?;
        Ok(items
            .iter()
            .filter(|item| item.get("title").and_then(|t| t.as_str()) == Some(title))
            .filter_map(|item| item.get("number").and_then(|n| n.as_u64()))
            .collect())
    }

    /// Comment bodies on an issue or a pull request thread, newest page last.
    ///
    /// The reconciliation for `comment_on_work`: the artifact is a comment,
    /// and the only way to know whether ours landed is to read them.
    pub async fn list_issue_comments(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<Vec<String>, GhError> {
        self.comment_bodies(&format!(
            "{}/repos/{owner}/{name}/issues/{number}/comments?per_page=100",
            self.rest_base_url
        ))
        .await
    }

    /// Review-comment bodies on a pull request's diff — the same question as
    /// [`Self::list_issue_comments`], on the other resource.
    pub async fn list_review_comments(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<Vec<String>, GhError> {
        self.comment_bodies(&format!(
            "{}/repos/{owner}/{name}/pulls/{number}/comments?per_page=100",
            self.rest_base_url
        ))
        .await
    }

    async fn comment_bodies(&self, url: &str) -> Result<Vec<String>, GhError> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let body = rest_ok(resp, "list comments").await?;
        let arr = body
            .as_array()
            .ok_or_else(|| GhError::Shape("comments response is not an array".into()))?;
        Ok(arr
            .iter()
            .filter_map(|c| Some(c.get("body")?.as_str()?.to_string()))
            .collect())
    }

    /// Merge a pull request. Returns the merge commit SHA.
    ///
    /// `method` is `merge`, `squash`, or `rebase`. GitHub refuses the call
    /// outright when the PR is not mergeable — behind a failing required
    /// check, conflicted, already merged — which is the behaviour we want:
    /// mergeability is a GitHub-owned fact, so it is asked at the moment of
    /// merging rather than read from anything we stored.
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        method: &str,
        commit_title: Option<&str>,
    ) -> Result<String, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/pulls/{number}/merge",
            self.rest_base_url
        );
        let mut payload = serde_json::json!({ "merge_method": method });
        if let Some(title) = commit_title {
            payload["commit_title"] = serde_json::Value::String(title.to_string());
        }
        let resp = self
            .http
            .put(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&payload)
            .send()
            .await?;
        let body = rest_ok(resp, &format!("merge pull request {number}")).await?;
        body.get("sha")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| GhError::Shape("merge response missing `sha`".into()))
    }

    /// Point a pull request at a different base branch (#1027).
    ///
    /// The verb the pipeline lacked. A build stacked on another build's branch
    /// is opened against that branch, and when the base lands *first* the
    /// dependent is left pointing at a branch nothing will pick up: merging it
    /// ships nothing, and it can never be retargeted afterwards, because GitHub
    /// refuses to edit a merged pull request. So the diagnosis had no act
    /// behind it and the instructed default was the irreversible one.
    ///
    /// REST rather than GraphQL for the reason PR creation is: the field is on
    /// `PATCH /repos/{o}/{r}/pulls/{n}`, and GitHub refuses the edit itself
    /// when the new base does not exist or the pull request is already merged —
    /// which is the check we want, asked at the moment of asking.
    pub async fn retarget_pull_request(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        base: &str,
    ) -> Result<String, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls/{number}", self.rest_base_url);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "base": base }))
            .send()
            .await?;
        let body = rest_ok(resp, &format!("retarget pull request {number}")).await?;
        // Read the base back rather than echoing the request: GitHub is the
        // only thing that knows whether the edit took, and a caller told what
        // it asked for has learned nothing.
        body.get("base")
            .and_then(|b| b.get("ref"))
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .ok_or_else(|| GhError::Shape("retarget response missing `base.ref`".into()))
    }

    /// Close a pull request without merging it.
    ///
    /// Distinct from [`GitHubClient::close_issue`] because PRs live under
    /// `/pulls` and take no `state_reason` — the reason belongs in a comment,
    /// which is why this is worth having alongside `create_issue_comment`
    /// rather than instead of it.
    pub async fn close_pull_request(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<(), GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls/{number}", self.rest_base_url);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await?;
        rest_ok(resp, &format!("close pull request {number}")).await?;
        Ok(())
    }

    /// Fetch all OPEN issues for a repository, paging as needed.
    pub async fn list_open_issues(&self, owner: &str, name: &str) -> Result<Vec<GhIssue>, GhError> {
        let mut issues = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (batch, next) = self.fetch_page(owner, name, cursor.as_deref()).await?;
            issues.extend(batch);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        debug!(owner, name, count = issues.len(), "fetched open issues");
        Ok(issues)
    }

    /// Look up state + close reason for specific issue numbers, batched via
    /// GraphQL aliases. Issues that don't resolve (deleted, converted to a
    /// discussion, transferred) are simply absent from the returned map.
    ///
    /// GitHub answers an unresolvable alias with a null node *and* an errors
    /// entry, so unlike [`Self::list_open_issues`] this tolerates errors as
    /// long as `data.repository` came back — otherwise one dead number would
    /// poison the whole batch.
    pub async fn issue_close_info(
        &self,
        owner: &str,
        name: &str,
        numbers: &[u64],
    ) -> Result<std::collections::HashMap<u64, IssueCloseInfo>, GhError> {
        let mut out = std::collections::HashMap::new();
        for chunk in numbers.chunks(CLOSE_INFO_BATCH) {
            let fields: String = chunk
                .iter()
                .map(|n| format!("i{n}: issue(number: {n}) {{ number state stateReason }}\n"))
                .collect();
            let query = format!(
                "query($owner: String!, $name: String!) {{\n\
                   repository(owner: $owner, name: $name) {{\n{fields}}}\n}}"
            );
            let body = serde_json::json!({
                "query": query,
                "variables": { "owner": owner, "name": name },
            });

            let resp: serde_json::Value = self
                .http
                .post(&self.base_url)
                .bearer_auth(self.token().expose())
                .header("Accept", "application/json")
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let repository = resp.pointer("/data/repository").filter(|v| !v.is_null());
            let Some(repository) = repository else {
                if let Some(errs) = resp.get("errors").and_then(|e| e.as_array()) {
                    let msg = errs
                        .iter()
                        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(GhError::GraphQl(msg));
                }
                return Err(GhError::Shape("repository is null".into()));
            };
            let nodes = repository
                .as_object()
                .ok_or_else(|| GhError::Shape("repository is not an object".into()))?;

            for (alias, node) in nodes {
                if node.is_null() {
                    warn!(alias, "issue did not resolve; leaving it out");
                    continue;
                }
                let number = node
                    .get("number")
                    .and_then(|n| n.as_u64())
                    .ok_or_else(|| GhError::Shape(format!("{alias}: number missing")))?;
                let state = match node.get("state").and_then(|s| s.as_str()) {
                    Some("OPEN") => GhState::Open,
                    _ => GhState::Closed,
                };
                let state_reason = node
                    .get("stateReason")
                    .and_then(|r| r.as_str())
                    .map(str::to_owned);
                out.insert(
                    number,
                    IssueCloseInfo {
                        state,
                        state_reason,
                    },
                );
            }
        }
        debug!(
            owner,
            name,
            asked = numbers.len(),
            resolved = out.len(),
            "fetched issue close info"
        );
        Ok(out)
    }

    /// How a pull request looks right now. Read at decision time, returned to
    /// the caller, and never persisted — `pr_number` is the only part of a PR
    /// this system owns.
    pub async fn pull_request_state(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<PrState, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls/{number}", self.rest_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(rest_error(format!("pull request {number}"), status, &body));
        }
        Ok(PrState {
            state: match body.get("state").and_then(|s| s.as_str()) {
                Some("open") => GhState::Open,
                _ => GhState::Closed,
            },
            merged: body
                .get("merged")
                .and_then(|m| m.as_bool())
                .unwrap_or(false),
            // Null while GitHub is still computing the merge commit; that is
            // "unknown", not "conflicted", and the distinction matters to a
            // reader deciding whether to act.
            mergeable: body.get("mergeable").and_then(|m| m.as_bool()),
            // The finer verdict, off the same body — no second request. This
            // is the field `landing()` reads first, because it is the only one
            // that can say "a required check has not passed".
            mergeable_state: body
                .get("mergeable_state")
                .and_then(|s| s.as_str())
                .map(str::to_owned),
            merge_commit_sha: body
                .get("merge_commit_sha")
                .and_then(|s| s.as_str())
                .map(str::to_owned),
            base_ref: body
                .get("base")
                .and_then(|base| base.get("ref"))
                .and_then(|s| s.as_str())
                .map(str::to_owned),
            head_ref: body
                .get("head")
                .and_then(|head| head.get("ref"))
                .and_then(|s| s.as_str())
                .map(str::to_owned),
        })
    }

    /// The open pull requests whose **base** is `branch` — everything stacked
    /// directly on it right now.
    ///
    /// Asked before a squash, and only before a squash (#1044). A merge commit
    /// leaves `branch` an ancestor of the trunk forever; a squash writes one
    /// new commit and leaves it an ancestor of nothing, so every pull request
    /// in this list would be left both undiagnosable by ancestry ("has my base
    /// landed?" answers no, and always will) and unretargetable at the trunk
    /// (replaying it there would replay the base's own commits). Neither a
    /// merge nor a retarget recovers them — only a rebase or a rebuild, and
    /// nothing in this pipeline can perform either.
    ///
    /// Numbers only: the caller names them in a refusal, and nothing here
    /// needs the bodies.
    pub async fn open_pull_requests_based_on(
        &self,
        owner: &str,
        name: &str,
        branch: &str,
    ) -> Result<Vec<u64>, GhError> {
        let url = format!("{}/repos/{owner}/{name}/pulls", self.rest_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .query(&[("state", "open"), ("base", branch), ("per_page", "100")])
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(rest_error(
                format!("open pull requests based on {branch}"),
                status,
                &body,
            ));
        }
        Ok(body
            .as_array()
            .map(|prs| {
                prs.iter()
                    .filter_map(|pr| pr.get("number").and_then(|n| n.as_u64()))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Is `sha` an ancestor of `trunk` — i.e. did that commit actually reach
    /// the branch that ships?
    ///
    /// `GET /compare/{base}...{head}` reads **`head` relative to `base`**, so
    /// with `base = trunk` and `head = sha` the answer is `identical` when the
    /// trunk tip *is* that commit and `behind` when the trunk has moved on
    /// past it. Anything else — `ahead` (the commit is off to one side),
    /// `diverged` — means it is not on the trunk. **Reversing the operands
    /// inverts the verdict**, which would silently close exactly the issues
    /// that should stay open.
    ///
    /// This exists because `merged` only ever meant "reached its base", and
    /// the pipeline stacks builds routinely.
    pub async fn merge_reached_trunk(
        &self,
        owner: &str,
        name: &str,
        trunk: &str,
        sha: &str,
    ) -> Result<bool, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/compare/{trunk}...{sha}",
            self.rest_base_url
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(rest_error(
                format!("compare {trunk}...{sha}"),
                status,
                &body,
            ));
        }
        let compare_status = body.get("status").and_then(|s| s.as_str());
        debug!(owner, name, trunk, sha, status = compare_status, "compared");
        Ok(matches!(compare_status, Some("identical") | Some("behind")))
    }

    /// Names of the entries directly inside `path` on `git_ref`.
    ///
    /// A missing directory is an empty listing rather than an error: the
    /// caller asks in order to compare against what is already there, and
    /// "nothing is there" is a perfectly good answer.
    pub async fn list_directory(
        &self,
        owner: &str,
        name: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Vec<String>, GhError> {
        let url = format!(
            "{}/repos/{owner}/{name}/contents/{path}?ref={git_ref}",
            self.rest_base_url
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(rest_error(format!("contents {path}"), status, &body));
        }
        // A file rather than a directory answers with an object, not an array.
        let Some(entries) = body.as_array() else {
            return Ok(Vec::new());
        };
        Ok(entries
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
            .map(str::to_owned)
            .collect())
    }

    /// Who this token is, and what it may do.
    ///
    /// One GraphQL round trip that names **no repository** — deliberately the
    /// cheapest call that keeps the three answers a diagnostic needs apart:
    /// the token is good (a login comes back), GitHub rejected it (a 401 with
    /// GitHub's own `message`, which `is_unavailable` reads as *answering*),
    /// or GitHub is unreachable (no answer at all). Nothing else in this file
    /// can do that — every other call names a repository, and a 404 there is
    /// ambiguous between "no such repo" and "this token cannot see it".
    ///
    /// The status is checked through [`rest_ok`] rather than
    /// `error_for_status`, so a revoked token reports "Bad credentials"
    /// instead of a naked `401 Unauthorized`.
    ///
    /// `errors` is read **before** the `/data/viewer` pointer, and the order is
    /// load-bearing: a bad credential answers HTTP **200** with
    /// `data.viewer: null` and its reason only in `errors`, so a pointer-first
    /// read reports "unexpected response shape" where GitHub said "Bad
    /// credentials" — and that sentence is the whole value of the failure to
    /// whoever is looking at a placeholder avatar or a red `doctor` line.
    ///
    /// All three identity fields or [`GhError::Shape`]: a half-identity dies
    /// here rather than arriving at a renderer as an avatar with no profile to
    /// open. GitHub's schema declares `avatarUrl` and `url` non-null on `User`,
    /// so this cannot fail against a GitHub that answered at all.
    pub(crate) async fn viewer(&self) -> Result<GhViewer, GhError> {
        let body = serde_json::json!({ "query": "query { viewer { login avatarUrl url } }" });
        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let graphql_scopes = scopes_from(resp.headers());
        let body = rest_ok(resp, "viewer").await?;

        // Errors first — see the doc comment. A 200 whose `errors` explain a
        // rejected credential must not be reported as a shape problem.
        if let Some(errs) = body.get("errors").and_then(|e| e.as_array())
            && !errs.is_empty()
        {
            let msg = errs
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::GraphQl(msg));
        }

        let field = |name: &str| -> Option<String> {
            body.pointer(&format!("/data/viewer/{name}"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        let (Some(login), Some(avatar_url), Some(profile_url)) =
            (field("login"), field("avatarUrl"), field("url"))
        else {
            return Err(GhError::Shape(
                "viewer.login, viewer.avatarUrl or viewer.url is absent".into(),
            ));
        };

        let (scopes, scope_source) = match graphql_scopes {
            Some(scopes) => (Some(scopes), Some(ScopeSource::GraphQlHeader)),
            None => match self.scopes_from_rest().await {
                Some(scopes) => (Some(scopes), Some(ScopeSource::RestHeader)),
                None => (None, None),
            },
        };
        Ok(GhViewer {
            login,
            avatar_url,
            profile_url,
            scopes,
            scope_source,
        })
    }

    /// The documented place to read `x-oauth-scopes`: a REST response.
    ///
    /// This second call exists because the first one's premise is *unverified*.
    /// GitHub documents the header on REST — `gh auth status` reads it off
    /// `GET /` for exactly this purpose — and documents it nowhere for
    /// GraphQL; what the GraphQL endpoint does volunteer is an
    /// `access-control-expose-headers` list naming `X-OAuth-Scopes`, which is
    /// suggestive and is not the same as observing one. So the GraphQL header
    /// is *used* when present and never *relied on*, because the failure of
    /// relying on it is silent: absence would read as the fine-grained-PAT
    /// case for every token including classic ones, and "not enumerable" is a
    /// verdict nobody re-investigates.
    ///
    /// `/rate_limit` rather than the API root, because it is documented as not
    /// counting against the rate limit — a diagnostic should not spend the
    /// budget it is reporting on. A failure here is `None`, not an error: the
    /// token has already been judged by the call above, and scopes are the
    /// part of this answer that is allowed to be missing.
    async fn scopes_from_rest(&self) -> Option<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/rate_limit", self.rest_base_url))
            .bearer_auth(self.token().expose())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .ok()?;
        scopes_from(resp.headers())
    }

    async fn fetch_page(
        &self,
        owner: &str,
        name: &str,
        after: Option<&str>,
    ) -> Result<(Vec<GhIssue>, Option<String>), GhError> {
        let query = r#"
        query($owner: String!, $name: String!, $after: String, $first: Int!, $labelFirst: Int!) {
          repository(owner: $owner, name: $name) {
            issues(states: [OPEN], first: $first, after: $after, orderBy: { field: UPDATED_AT, direction: DESC }) {
              pageInfo { hasNextPage endCursor }
              nodes {
                number
                title
                body
                state
                updatedAt
                labels(first: $labelFirst) { nodes { name } }
              }
            }
          }
        }
        "#;

        let req_body = GraphQlRequest {
            query,
            variables: Variables {
                owner,
                name,
                after,
                first: PAGE_SIZE,
                label_first: LABEL_PAGE_SIZE,
            },
        };

        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(self.token().expose())
            .header("Accept", "application/json")
            .json(&req_body)
            .send()
            .await?
            .error_for_status()?
            .json::<GraphQlResponse>()
            .await?;

        if let Some(errs) = resp.errors {
            let msg = errs
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::GraphQl(msg));
        }

        let data = resp
            .data
            .ok_or_else(|| GhError::Shape("response missing `data` field".into()))?;
        let issues = data
            .repository
            .ok_or_else(|| GhError::Shape("repository is null".into()))?
            .issues;

        let nodes = issues
            .nodes
            .into_iter()
            .map(normalize_issue)
            .collect::<Result<Vec<_>, _>>()?;
        let next = if issues.page_info.has_next_page {
            issues.page_info.end_cursor
        } else {
            None
        };
        Ok((nodes, next))
    }
}

fn normalize_issue(raw: RawIssue) -> Result<GhIssue, GhError> {
    let state = match raw.state.as_str() {
        "OPEN" => GhState::Open,
        "CLOSED" => GhState::Closed,
        other => {
            warn!(state = other, "unknown issue state, treating as open");
            GhState::Open
        }
    };
    let updated_at = DateTime::parse_from_rfc3339(&raw.updated_at)
        .map_err(|e| GhError::Shape(format!("updatedAt parse: {e}")))?
        .with_timezone(&Utc);
    let labels = raw
        .labels
        .map(|l| l.nodes.into_iter().map(|n| n.name).collect())
        .unwrap_or_default();

    Ok(GhIssue {
        number: raw.number,
        title: raw.title,
        body: raw.body.unwrap_or_default(),
        labels,
        state,
        updated_at,
    })
}

// --- wire types ---

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
    variables: Variables<'a>,
}

#[derive(Serialize)]
struct Variables<'a> {
    owner: &'a str,
    name: &'a str,
    after: Option<&'a str>,
    first: u32,
    #[serde(rename = "labelFirst")]
    label_first: u32,
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
    #[serde(default)]
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct GraphQlData {
    repository: Option<RepoData>,
}

#[derive(Deserialize)]
struct RepoData {
    issues: IssuesConn,
}

#[derive(Deserialize)]
struct IssuesConn {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    labels: Option<LabelConn>,
}

#[derive(Deserialize)]
struct LabelConn {
    nodes: Vec<LabelNode>,
}

#[derive(Deserialize)]
struct LabelNode {
    name: String,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::response::Json as AxumJson;
    use axum::routing::post;
    use serde_json::{Value, json};

    use super::*;

    /// Spawn an axum server that returns responses from a queue. Each POST
    /// pops the next response; if the queue is empty, returns an empty data
    /// block. Returns (base_url, handle).
    async fn spawn_fake(
        responses: Vec<Value>,
    ) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let queue = Arc::new(Mutex::new(responses));
        let state = queue.clone();
        let app = Router::new()
            .route(
                "/graphql",
                post(move |State(q): State<Arc<Mutex<Vec<Value>>>>, _body: String| async move {
                    let resp = {
                        let mut g = q.lock().unwrap();
                        if g.is_empty() {
                            json!({"data": {"repository": {"issues": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []}}}})
                        } else {
                            g.remove(0)
                        }
                    };
                    AxumJson(resp)
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/graphql");
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, queue, handle)
    }

    /// A GraphQL responder that also sets response headers, so the scope read
    /// can be exercised in both of its shapes. Returns (graphql url, rest
    /// root) — `viewer` falls back to `GET /rate_limit` under the REST root
    /// when the GraphQL response carries no `x-oauth-scopes`.
    async fn spawn_viewer_fake(
        graphql: Value,
        graphql_status: u16,
        graphql_scopes: Option<&str>,
        rest_scopes: Option<&str>,
    ) -> (String, String) {
        use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
        use axum::routing::get;

        fn header_map(scopes: Option<&str>) -> HeaderMap {
            let mut headers = HeaderMap::new();
            if let Some(scopes) = scopes {
                headers.insert(
                    HeaderName::from_static("x-oauth-scopes"),
                    HeaderValue::from_str(scopes).unwrap(),
                );
            }
            headers
        }

        let gql_headers = header_map(graphql_scopes);
        let rest_headers = header_map(rest_scopes);
        let status = StatusCode::from_u16(graphql_status).unwrap();
        let app = Router::new()
            .route(
                "/graphql",
                post(move |_body: String| async move {
                    (status, gql_headers.clone(), AxumJson(graphql.clone()))
                }),
            )
            .route(
                "/rate_limit",
                get(move || async move { (rest_headers.clone(), AxumJson(json!({"rate": {}}))) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/graphql"), format!("http://{addr}"))
    }

    /// A complete `viewer` payload. Every identity field is required, so the
    /// scope tests below say so once here rather than each restating it.
    fn viewer_body(login: &str) -> Value {
        json!({"data": {"viewer": {
            "login": login,
            "avatarUrl": format!("https://avatars.example/{login}.png"),
            "url": format!("https://github.example/{login}"),
        }}})
    }

    #[tokio::test]
    async fn viewer_reads_the_login_and_splits_the_scope_header() {
        let (gql, rest) = spawn_viewer_fake(
            viewer_body("iamnbutler"),
            200,
            Some("repo, read:org , workflow"),
            None,
        )
        .await;
        let viewer = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap();
        assert_eq!(viewer.login, "iamnbutler");
        assert_eq!(
            viewer.scopes.as_deref(),
            Some(["repo".to_string(), "read:org".into(), "workflow".into()].as_slice())
        );
        assert_eq!(viewer.scope_source, Some(ScopeSource::GraphQlHeader));
    }

    /// The premise this fallback exists for. `x-oauth-scopes` is documented on
    /// REST and nowhere for GraphQL, so a GraphQL response without it must not
    /// end the question — reading absence there as "this token has no scopes
    /// to enumerate" would report every classic token that way.
    #[tokio::test]
    async fn scopes_fall_back_to_the_documented_rest_header() {
        let (gql, rest) = spawn_viewer_fake(viewer_body("someone"), 200, None, Some("repo")).await;
        let viewer = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap();
        assert_eq!(
            viewer.scopes.as_deref(),
            Some(["repo".to_string()].as_slice())
        );
        assert_eq!(viewer.scope_source, Some(ScopeSource::RestHeader));
    }

    /// `None` and `Some(vec![])` are opposite verdicts and must stay apart the
    /// whole way out: absent means a fine-grained PAT or an App token, which
    /// has permissions rather than scopes; empty means a token that can do
    /// nothing. Reading the first as the second tells an operator to replace a
    /// token that works.
    #[tokio::test]
    async fn an_absent_scope_header_is_not_an_empty_one() {
        let (gql, rest) = spawn_viewer_fake(viewer_body("app"), 200, None, None).await;
        let absent = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap();
        assert_eq!(absent.scopes, None);
        assert_eq!(absent.scope_source, None);

        let (gql, rest) = spawn_viewer_fake(viewer_body("app"), 200, Some(""), None).await;
        let empty = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap();
        assert_eq!(empty.scopes, Some(Vec::new()));
    }

    /// A rejected token is GitHub *answering*, in either of its two error
    /// shapes — a 401 body and a 200 with an `errors` block. Neither may read
    /// as an outage, or a diagnostic would report a revoked credential as "we
    /// could not tell" and hold dispatch for it.
    #[tokio::test]
    async fn a_rejected_token_is_not_an_outage_in_either_shape() {
        let (gql, rest) =
            spawn_viewer_fake(json!({"message": "Bad credentials"}), 401, None, None).await;
        let err = GitHubClient::with_base_url("bad", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap_err();
        assert!(!err.is_unavailable(), "{err}");
        assert!(err.to_string().contains("Bad credentials"), "{err}");

        let (gql, rest) = spawn_viewer_fake(
            json!({"errors": [{"message": "Bad credentials"}]}),
            200,
            None,
            None,
        )
        .await;
        let err = GitHubClient::with_base_url("bad", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap_err();
        assert!(!err.is_unavailable(), "{err}");
        assert!(err.to_string().contains("Bad credentials"), "{err}");
    }

    /// ...and a 5xx *is* one, decided structurally off the status rather than
    /// off the message text.
    #[tokio::test]
    async fn a_5xx_is_an_outage() {
        let (gql, rest) =
            spawn_viewer_fake(json!({"message": "unavailable"}), 503, None, None).await;
        let err = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap_err();
        assert!(err.is_unavailable(), "{err}");
    }

    /// The identity half of the same round trip: the login, the avatar and the
    /// profile link all come **off the wire**. `profile_url` in particular is
    /// GitHub's own `url` and is never assembled from the login — on an
    /// Enterprise host the origin is not github.com, and guessing it is the
    /// same class of mistake that hardcoded one maintainer's profile (#987).
    #[tokio::test]
    async fn viewer_reads_the_avatar_and_the_profile_link_off_the_wire() {
        let (gql, rest) = spawn_viewer_fake(
            json!({"data": {"viewer": {
                "login": "octocat",
                "avatarUrl": "https://avatars.enterprise.example/u/9",
                "url": "https://github.enterprise.example/octocat",
            }}}),
            200,
            Some("repo"),
            None,
        )
        .await;
        let viewer = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap();
        assert_eq!(viewer.login, "octocat");
        assert_eq!(viewer.avatar_url, "https://avatars.enterprise.example/u/9");
        assert_eq!(
            viewer.profile_url,
            "https://github.enterprise.example/octocat"
        );
    }

    /// A half-identity dies here rather than reaching a renderer as an avatar
    /// with no profile to open.
    #[tokio::test]
    async fn a_viewer_missing_a_field_is_a_shape_error() {
        let (gql, rest) = spawn_viewer_fake(
            json!({"data": {"viewer": {"login": "octocat", "avatarUrl": "https://a/1"}}}),
            200,
            Some("repo"),
            None,
        )
        .await;
        let err = GitHubClient::with_base_url("token", gql)
            .with_rest_base_url(rest)
            .viewer()
            .await
            .unwrap_err();
        assert!(matches!(err, GhError::Shape(_)), "{err}");
    }

    fn page(nodes: Vec<Value>, has_next: bool, end_cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": {
                            "hasNextPage": has_next,
                            "endCursor": end_cursor,
                        },
                        "nodes": nodes,
                    }
                }
            }
        })
    }

    fn issue(number: u64, title: &str, labels: &[&str]) -> Value {
        json!({
            "number": number,
            "title": title,
            "body": format!("body of {number}"),
            "state": "OPEN",
            "updatedAt": "2026-04-17T00:00:00Z",
            "labels": {
                "nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            }
        })
    }

    /// `create_issue` was the one write not going through [`rest_ok`]: it
    /// parsed the body *before* checking the status, so a 5xx whose body is
    /// not JSON — a proxy's error page — became `GhError::Http(decode error)`,
    /// which carries no status. [`GhError::is_unavailable`] then read it as
    /// GitHub *answering*, and the intent ledger would have **annulled** a
    /// decision whose issue may well have been filed.
    ///
    /// Found only because the all-routes guard in `tests/custodial.rs` drove
    /// every route through a failing GitHub; a route-by-route test would have
    /// missed it.
    #[tokio::test]
    async fn a_five_hundred_with_an_unparseable_body_reads_as_an_outage() {
        let app = Router::new().fallback(|| async {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                [(axum::http::header::CONTENT_TYPE, "text/html")],
                "<html><body>502 Bad Gateway</body></html>",
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GitHubClient::with_base_url("token", "http://unused.invalid/graphql")
            .with_rest_base_url(base);
        let err = client
            .create_issue("own", "repo", "a title", "a body", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, GhError::Rest { status, .. } if status.is_server_error()),
            "the status is what decides, not the body: {err}"
        );
        assert!(
            err.is_unavailable(),
            "GitHub did not answer, and a decision about this issue must stay pending"
        );
    }

    #[tokio::test]
    async fn list_open_issues_single_page() {
        let responses = vec![page(
            vec![issue(1, "first", &["bug"]), issue(2, "second", &[])],
            false,
            None,
        )];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[0].title, "first");
        assert_eq!(issues[0].labels, vec!["bug".to_string()]);
        assert_eq!(issues[0].state, GhState::Open);
        assert_eq!(issues[1].labels, Vec::<String>::new());
    }

    #[tokio::test]
    async fn list_open_issues_paginates() {
        let responses = vec![
            page(vec![issue(1, "a", &[])], true, Some("cursor1")),
            page(vec![issue(2, "b", &[])], false, None),
        ];
        let (url, queue, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].number, 2);
        // Both canned pages consumed
        assert!(queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn graphql_errors_propagate() {
        let responses = vec![json!({
            "errors": [{"message": "Bad credentials"}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let err = client.list_open_issues("own", "repo").await.unwrap_err();
        match err {
            GhError::GraphQl(msg) => assert!(msg.contains("Bad credentials")),
            other => panic!("expected GraphQl error, got {other:?}"),
        }
    }

    /// GitHub answers an unresolvable issue alias with a null node plus an
    /// errors entry; the resolvable siblings in the same batch must survive.
    #[tokio::test]
    async fn issue_close_info_parses_aliases_and_tolerates_dead_numbers() {
        let responses = vec![json!({
            "data": {
                "repository": {
                    "i1": {"number": 1, "state": "CLOSED", "stateReason": "COMPLETED"},
                    "i2": {"number": 2, "state": "CLOSED", "stateReason": "NOT_PLANNED"},
                    "i3": {"number": 3, "state": "OPEN", "stateReason": null},
                    "i4": null,
                }
            },
            "errors": [{"message": "Could not resolve to an Issue with the number of 4."}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let info = client
            .issue_close_info("own", "repo", &[1, 2, 3, 4])
            .await
            .unwrap();

        assert_eq!(
            info.get(&1),
            Some(&IssueCloseInfo {
                state: GhState::Closed,
                state_reason: Some("COMPLETED".into()),
            })
        );
        assert_eq!(
            info.get(&2).and_then(|i| i.state_reason.as_deref()),
            Some("NOT_PLANNED")
        );
        assert_eq!(info.get(&3).map(|i| i.state), Some(GhState::Open));
        assert!(!info.contains_key(&4), "dead number stays absent");
    }

    #[tokio::test]
    async fn issue_close_info_fails_when_data_is_absent() {
        let responses = vec![json!({
            "errors": [{"message": "Bad credentials"}]
        })];
        let (url, _q, _h) = spawn_fake(responses).await;

        let client = GitHubClient::with_base_url("token", url);
        let err = client
            .issue_close_info("own", "repo", &[1])
            .await
            .unwrap_err();
        match err {
            GhError::GraphQl(msg) => assert!(msg.contains("Bad credentials")),
            other => panic!("expected GraphQl error, got {other:?}"),
        }
    }

    /// The default filter is a no-op: every deployment that never sets
    /// `TASKS_INTAKE_LABEL` keeps ingesting everything.
    #[tokio::test]
    async fn unset_filter_admits_every_issue() {
        let responses = vec![page(
            vec![
                issue(1, "labelled", &["tasks"]),
                issue(2, "bare", &[]),
                issue(3, "other", &["bug", "docs"]),
            ],
            false,
            None,
        )];
        let (url, _q, _h) = spawn_fake(responses).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        let filter = IntakeFilter::default();
        assert_eq!(filter, IntakeFilter::All);
        assert!(issues.iter().all(|i| filter.admits(i)));
        assert_eq!(filter.label(), None);
    }

    #[tokio::test]
    async fn label_filter_admits_only_labelled_issues() {
        let responses = vec![page(
            vec![
                issue(1, "labelled", &["tasks"]),
                issue(2, "bare", &[]),
                issue(3, "other labels", &["bug", "docs"]),
                issue(4, "labelled among others", &["bug", "tasks"]),
            ],
            false,
            None,
        )];
        let (url, _q, _h) = spawn_fake(responses).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        let filter = IntakeFilter::from_label(Some("tasks".into()));
        let admitted: Vec<u64> = issues
            .iter()
            .filter(|i| filter.admits(i))
            .map(|i| i.number)
            .collect();
        assert_eq!(admitted, vec![1, 4]);
        assert_eq!(filter.label(), Some("tasks"));
    }

    /// GitHub itself refuses two labels differing only in ASCII case, so a
    /// case-sensitive match would only ever be a configuration footgun.
    #[tokio::test]
    async fn label_matching_is_case_insensitive() {
        let responses = vec![page(vec![issue(1, "shouty", &["TASKS"])], false, None)];
        let (url, _q, _h) = spawn_fake(responses).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        assert!(IntakeFilter::from_label(Some("tasks".into())).admits(&issues[0]));
        assert!(IntakeFilter::from_label(Some("Tasks".into())).admits(&issues[0]));
    }

    /// A blank or whitespace-only value means "unset". The alternative — a label
    /// no issue can carry — would silently halt all intake.
    #[tokio::test]
    async fn blank_label_reads_as_unset() {
        let responses = vec![page(vec![issue(1, "bare", &[])], false, None)];
        let (url, _q, _h) = spawn_fake(responses).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();

        for raw in [None, Some(String::new()), Some("   ".into())] {
            let filter = IntakeFilter::from_label(raw);
            assert_eq!(filter, IntakeFilter::All);
            assert!(filter.admits(&issues[0]));
        }
        // Surrounding whitespace on a real label is trimmed, not honoured.
        assert_eq!(
            IntakeFilter::from_label(Some("  tasks \n".into())),
            IntakeFilter::Label("tasks".into())
        );
    }

    #[tokio::test]
    async fn empty_repository_returns_empty_list() {
        let (url, _q, _h) = spawn_fake(vec![]).await;
        let client = GitHubClient::with_base_url("token", url);
        let issues = client.list_open_issues("own", "repo").await.unwrap();
        assert!(issues.is_empty());
    }

    fn pr(state: GhState, merged: bool, mergeable: Option<bool>, ms: Option<&str>) -> PrState {
        PrState {
            state,
            merged,
            mergeable,
            mergeable_state: ms.map(str::to_owned),
            merge_commit_sha: None,
            base_ref: Some("main".into()),
            head_ref: Some("build/example".into()),
        }
    }

    /// The reason `mergeable_state` is read at all: a pull request behind a
    /// failing *required* check answers `mergeable: true`, so the coarse flag
    /// alone reads it as ready.
    #[test]
    fn a_blocked_pr_is_blocked_however_mergeable_the_coarse_flag_says_it_is() {
        assert_eq!(
            pr(GhState::Open, false, Some(true), Some("blocked")).landing(),
            Landing::Blocked(BLOCKED_REQUIRED)
        );
    }

    #[test]
    fn each_mergeable_state_maps_to_one_verdict() {
        let landing = |state| pr(GhState::Open, false, Some(true), Some(state)).landing();
        assert_eq!(landing("clean"), Landing::Clear);
        assert_eq!(landing("unstable"), Landing::Unstable);
        assert_eq!(landing("dirty"), Landing::Blocked(BLOCKED_CONFLICT));
        assert_eq!(landing("draft"), Landing::Blocked(BLOCKED_DRAFT));
        assert_eq!(landing("behind"), Landing::Blocked(BLOCKED_BEHIND));
    }

    /// Unknown is its own answer. GitHub computes mergeability lazily, so the
    /// seconds after a push read exactly like this — and reporting them as a
    /// block would park work on a verdict that has not been rendered.
    #[test]
    fn an_uncomputed_verdict_is_unknown_rather_than_blocked() {
        assert_eq!(
            pr(GhState::Open, false, None, Some("unknown")).landing(),
            Landing::Unknown
        );
        assert_eq!(
            pr(GhState::Open, false, None, None).landing(),
            Landing::Unknown
        );
        // A state nobody here has heard of falls back rather than guessing.
        assert_eq!(
            pr(GhState::Open, false, None, Some("has_hooks")).landing(),
            Landing::Unknown
        );
        assert!(
            Landing::Unknown.describe().contains("not a block"),
            "{}",
            Landing::Unknown.describe()
        );
    }

    /// With no `mergeable_state` at all — an older API, a partial body — the
    /// coarse flag is all there is, and it still has to answer something.
    #[test]
    fn the_coarse_flag_is_the_fallback_and_only_the_fallback() {
        assert_eq!(
            pr(GhState::Open, false, Some(true), None).landing(),
            Landing::Clear
        );
        assert_eq!(
            pr(GhState::Open, false, Some(false), None).landing(),
            Landing::Blocked(BLOCKED_CONFLICT)
        );
    }

    /// GitHub keeps answering `mergeable_state` on a closed pull request, and
    /// that answer is about a merge that can no longer be made.
    #[test]
    fn merged_and_closed_outrank_any_mergeability() {
        assert_eq!(
            pr(GhState::Closed, true, Some(false), Some("dirty")).landing(),
            Landing::Merged
        );
        assert_eq!(
            pr(GhState::Closed, false, Some(true), Some("clean")).landing(),
            Landing::ClosedUnmerged
        );
        // And `merged` still means "reached its base", nothing more.
        assert!(
            Landing::Merged.describe().contains("not the same as"),
            "{}",
            Landing::Merged.describe()
        );
    }

    /// The wording is pinned because it is the load-bearing part, and #1015
    /// moved what it has to say without moving how much it may claim.
    ///
    /// `clean` now means CI passed on the head commit, so the old sentence
    /// ("no check here is capable of objecting") is false and had to go. What
    /// replaces it is the narrower true one — this run covered the branch
    /// against its own base and not the composition — because a `describe()`
    /// that read as a clearance would be instructing every future turn to land
    /// on evidence it does not have. The forbidden list is unchanged: the
    /// failure mode is a reader upgrading "nothing in the way" into "ready".
    #[test]
    fn clear_says_what_it_does_not_mean() {
        let said = Landing::Clear.describe();
        assert!(
            said.contains("composed with a trunk that has moved"),
            "{said}"
        );
        for forbidden in ["good to merge", "ready to merge", "safe"] {
            assert!(!said.contains(forbidden), "{forbidden:?} in: {said}");
        }
        // Blocked names its reason and says the call itself would fail.
        let blocked = Landing::Blocked(BLOCKED_REQUIRED).describe();
        assert!(blocked.contains(BLOCKED_REQUIRED), "{blocked}");
        assert!(blocked.contains("would be refused"), "{blocked}");
    }

    /// The half of #1015 that is easiest to get wrong. `unstable` conflates a
    /// check that FAILED with one that has not finished, GitHub will not say
    /// which, and nothing refuses the merge — so a sentence that filed it under
    /// non-required noise (which is what it said before CI existed) is an
    /// instruction to merge red work.
    #[test]
    fn unstable_says_a_check_may_be_red_and_that_nothing_will_stop_you() {
        let said = Landing::Unstable.describe();
        assert!(said.contains("FAILING"), "{said}");
        assert!(said.contains("has not finished"), "{said}");
        assert!(said.contains("nothing will refuse the merge"), "{said}");
        assert!(
            !said.contains("says nothing about whether the change works"),
            "the pre-CI dismissal is exactly what must not survive: {said}"
        );
    }

    fn rest(status: u16, message: &str) -> GhError {
        rest_error(
            "read pull request 7",
            reqwest::StatusCode::from_u16(status).unwrap(),
            &json!({ "message": message }),
        )
    }

    /// The hold this feeds is only ever set by "GitHub is not answering". A
    /// 5xx is that; a 4xx is GitHub answering, and holding on one would be a
    /// hold nothing could clear.
    #[test]
    fn only_a_server_error_reads_as_unavailable() {
        for status in [500, 502, 503, 504] {
            assert!(rest(status, "unavailable").is_unavailable(), "{status}");
        }
        for status in [400, 401, 403, 404, 409, 422] {
            assert!(!rest(status, "nope").is_unavailable(), "{status}");
        }
        // 429 is a fact about our own usage — it names its own reset, and a
        // clone does not spend the API quota.
        assert!(!rest(429, "rate limited").is_unavailable());
        // Our own bug, and GraphQL errors are GitHub answering.
        assert!(!GhError::Shape("no `data`".into()).is_unavailable());
        assert!(!GhError::GraphQl("Bad credentials".into()).is_unavailable());
    }

    /// The variant's shape moved; its rendered text did not. This message is
    /// read by humans in warnings and in stored failure reasons.
    #[test]
    fn the_rest_message_is_byte_identical_to_what_it_always_was() {
        let err = rest(422, "Pull Request is not mergeable");
        assert_eq!(
            err.to_string(),
            "rest: read pull request 7: 422 Unprocessable Entity: Pull Request is not mergeable"
        );
        // A body with no `message` still renders the way it used to.
        let bare = rest_error(
            "create issue",
            reqwest::StatusCode::NOT_FOUND,
            &json!({ "documentation_url": "..." }),
        );
        assert_eq!(
            bare.to_string(),
            "rest: create issue: 404 Not Found: (no message)"
        );
    }

    /// Empirical, because the classification rests on reqwest carrying the
    /// status through `error_for_status()` — the GraphQL path's only failure
    /// mode. If that ever stopped being true, every outage would read as a
    /// transport error instead, and this is what would notice.
    #[tokio::test]
    async fn a_real_503_through_the_graphql_path_reads_as_unavailable() {
        let app = Router::new().route(
            "/graphql",
            post(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    AxumJson(json!({"message": "Service Unavailable"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GitHubClient::with_base_url("token", url);
        let err = client.list_open_issues("own", "repo").await.unwrap_err();
        assert!(matches!(err, GhError::Http(_)), "{err:?}");
        assert!(err.is_unavailable(), "{err}");
    }

    /// The other half of "never got a response": nothing is listening at all,
    /// which is what a poll during an outage most often looks like.
    #[tokio::test]
    async fn a_refused_connection_reads_as_unavailable() {
        // Bind, read the port, drop the listener: nothing can be listening
        // there, and the OS answers RST rather than making us wait.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/graphql", listener.local_addr().unwrap());
        drop(listener);

        let client = GitHubClient::with_base_url("token", url);
        let err = client.list_open_issues("own", "repo").await.unwrap_err();
        assert!(err.is_unavailable(), "{err}");
    }

    /// The field comes off the body `pull_request_state` already fetches — no
    /// second request, which is what makes putting it on the brief affordable.
    #[tokio::test]
    async fn pull_request_state_reads_mergeable_state_off_the_same_body() {
        let app = Router::new().route(
            "/repos/{owner}/{repo}/pulls/{number}",
            axum::routing::get(|| async {
                AxumJson(json!({
                    "state": "open",
                    "merged": false,
                    "mergeable": true,
                    "mergeable_state": "blocked",
                    "merge_commit_sha": "abc",
                    "base": { "ref": "main" },
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GitHubClient::new("token").with_rest_base_url(base);
        let state = client.pull_request_state("own", "repo", 7).await.unwrap();

        assert_eq!(state.mergeable_state.as_deref(), Some("blocked"));
        assert_eq!(state.landing(), Landing::Blocked(BLOCKED_REQUIRED));
        // …and the coarse label is unchanged: the two are read together.
        assert_eq!(state.label(), "open");
    }
}
