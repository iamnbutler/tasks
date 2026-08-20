//! The credential broker: short-lived leases, and the proxy that spends the
//! real keys so nothing else ever holds them.
//!
//! See `docs/plans/2026-08-18-credential-custody.md`. [`crate::secrets`] is
//! custody at rest; this module is custody in motion. What a VM receives at
//! dispatch is a **lease** — a random 256-bit bearer token, stored only as a
//! SHA-256 hash, bound to its run and its repo, scoped, and expiring at the
//! run's budget plus slack — and two pieces of configuration that point every
//! credentialed operation at this process:
//!
//! - `ANTHROPIC_API_KEY=<lease>` + `ANTHROPIC_BASE_URL=http://<host>:<port>/anthropic`
//!   (the variable name is kept so Claude Code needs no image change, and so
//!   #970's name-based redaction masks the lease everywhere it already masked
//!   the key);
//! - a clone URL of the form
//!   `http://x-access-token:<lease>@<host>:<port>/git/<owner>/<repo>.git`.
//!
//! The broker validates the lease on every request and forwards to the real
//! upstream with the real credential injected host-side, over TLS. The keys
//! never cross the vmnet in either direction; what a leaked VM environment,
//! `container run` argv, or transcript can yield is a token that stops
//! working minutes after its run concludes — and both redaction layers
//! (#970) scrub even that from logs, since it rides a URL userinfo and an
//! `*_KEY`-named variable.
//!
//! # Scopes are enforcement, not description
//!
//! The classic PAT upstream is all-or-nothing; the broker is what makes a
//! scoped credential out of it. Agent leases carry `anthropic` + `git-read`
//! and are bound to one repo: a Scout or Builder *cannot* push, and cannot
//! read a repo it was not dispatched for, whatever its prompt talks it into.
//! The push credential exists only as the server's own ~10-minute `land`
//! lease, minted per landing — so even host-side `git` argv never carries
//! the PAT.
//!
//! # Why leases are rows
//!
//! The process that mints a lease need not be the process serving it: a
//! server restart reattaches to running VMs, and their leases must keep
//! answering — [`LeaseIssuer::extend`] moves the expiry to the resumed
//! deadline by subject, without ever knowing the token. Conclusion revokes
//! best-effort; expiry is the backstop nothing can forget to apply.
//!
//! # The listener
//!
//! A second listener, deliberately not the API's: the API stays
//! loopback-only, while this one is reachable from the VM subnet
//! (`TASKS_BROKER_BIND`, default all interfaces; VMs reach the host at
//! apple/container's bridge gateway, `TASKS_BROKER_ADVERTISE`, default
//! `192.168.64.1`). Every route demands a valid lease, so what the wider
//! bind exposes is a 401, never an unauthenticated capability.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, get, post};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::secrets::Secrets;
use crate::store::{Store, StoreError};

/// Slack added to a run's budget when its lease is minted: the lease must
/// outlive the agent by enough to cover allocation before the budget starts
/// counting and the drain after it stops. Generous, because the failure mode
/// of "too short" is an agent dying mid-run with a 401 that looks like an
/// upstream outage, and the cost of "too long" is minutes on a token that is
/// also revoked at conclusion.
pub const LEASE_SLACK: Duration = Duration::from_secs(15 * 60);

/// How long the server's own fetch+push window stays open when landing a
/// branch. One landing is seconds; ten minutes covers a slow fetch of a large
/// repo with room to spare.
const LAND_LEASE_TTL: Duration = Duration::from_secs(10 * 60);

/// Same argument, same number as `server::CONNECTION_GRACE`: a shutdown must
/// terminate, and an in-flight SSE response from the Anthropic upstream never
/// closes on the broker's schedule. Severing it is safe — the in-VM
/// supervisor already resumes an agent whose connection dropped (#845).
const CONNECTION_GRACE: Duration = Duration::from_secs(2);

/// What one lease may do. `allows` is the only reader; everything else treats
/// the set as opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scopes {
    pub anthropic: bool,
    pub git_read: bool,
    pub git_write: bool,
}

impl Scopes {
    /// A Scout's or Builder's lease: talk to Anthropic, read one repo. No
    /// write — branch egress is a bundle, and the push is the server's.
    pub const AGENT: Scopes = Scopes {
        anthropic: true,
        git_read: true,
        git_write: false,
    };
    /// The lease of a run whose clone is [`CloneSource::Direct`] (a `file://`
    /// mirror): the broker cannot front its repo, but its Anthropic credit
    /// still goes through here rather than riding raw.
    pub const ANTHROPIC_ONLY: Scopes = Scopes {
        anthropic: true,
        git_read: false,
        git_write: false,
    };
    /// The server's own landing window: fetch the base, push the branch.
    pub const LAND: Scopes = Scopes {
        anthropic: false,
        git_read: true,
        git_write: true,
    };

    pub fn allows(self, scope: Scope) -> bool {
        match scope {
            Scope::Anthropic => self.anthropic,
            Scope::GitRead => self.git_read,
            Scope::GitWrite => self.git_write,
        }
    }

    /// The store encoding: space-separated names, stable across versions.
    pub fn encode(self) -> String {
        let mut parts = Vec::new();
        if self.anthropic {
            parts.push("anthropic");
        }
        if self.git_read {
            parts.push("git-read");
        }
        if self.git_write {
            parts.push("git-write");
        }
        parts.join(" ")
    }

    /// Unknown words are ignored rather than erroring: an older binary
    /// reading a newer row must not turn a scope it cannot name into a
    /// refusal of the ones it can.
    pub fn decode(raw: &str) -> Scopes {
        let mut scopes = Scopes {
            anthropic: false,
            git_read: false,
            git_write: false,
        };
        for word in raw.split_whitespace() {
            match word {
                "anthropic" => scopes.anthropic = true,
                "git-read" => scopes.git_read = true,
                "git-write" => scopes.git_write = true,
                _ => {}
            }
        }
        scopes
    }
}

/// One permission being asked for, at one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Anthropic,
    GitRead,
    GitWrite,
}

/// What a lease was minted for. `Scout` subjects are session ids, `Build` and
/// `Land` subjects are build ids — `Land` separately from `Build` because the
/// two differ in every property that matters (scopes, TTL, which process
/// holds the token), and revoking a concluded build must be able to name
/// both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    Scout,
    Build,
    Land,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Scout => "scout",
            SubjectKind::Build => "build",
            SubjectKind::Land => "land",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        [SubjectKind::Scout, SubjectKind::Build, SubjectKind::Land]
            .into_iter()
            .find(|k| k.as_str() == raw)
    }
}

/// A lease row. The token itself is never stored and never reconstructible —
/// `token_hash` is its SHA-256.
#[derive(Clone, Debug)]
pub struct Lease {
    pub id: String,
    pub token_hash: String,
    pub scopes: Scopes,
    /// `owner/name` the git scopes are bound to.
    pub repo: Option<String>,
    pub subject_kind: SubjectKind,
    pub subject_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Lease {
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

/// Mint a fresh bearer token and its storable hash. The `tl-` prefix is for
/// humans reading a leak: it names what leaked (a tasks lease, dead minutes
/// later) rather than leaving a bare blob to be treated as a key.
fn mint_token() -> (String, String) {
    mint_with_prefix("tl")
}

/// Mint an agent enrollment code and its storable hash (`Store::enroll_agent`).
/// Same custody as a lease — 256 random bits, hash at rest — with its own
/// prefix so a leak names itself: a short-lived message code, not a key.
pub fn mint_agent_code() -> (String, String) {
    mint_with_prefix("ta")
}

fn mint_with_prefix(prefix: &str) -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = format!(
        "{prefix}-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    let hash = token_hash(&token);
    (token, hash)
}

/// SHA-256 of the presented token, hex — the only form a lease ever takes at
/// rest.
pub fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The VM-facing clone URL for a leased repo. Lease in the userinfo, which is
/// exactly what both redaction layers already scrub from every log path git
/// echoes a remote into.
pub fn vm_clone_url(advertise_host: &str, port: u16, repo: &str, token: &str) -> String {
    format!("http://x-access-token:{token}@{advertise_host}:{port}/git/{repo}.git")
}

/// The VM-facing Anthropic base URL.
pub fn anthropic_base_url(advertise_host: &str, port: u16) -> String {
    format!("http://{advertise_host}:{port}/anthropic")
}

/// The server's own clone URL for a landing: loopback, so the lease never
/// leaves the host at all.
fn land_clone_url(port: u16, repo: &str, token: &str) -> String {
    format!("http://x-access-token:{token}@127.0.0.1:{port}/git/{repo}.git")
}

/// Everything a dispatched agent VM needs to operate on credit: the env that
/// aims Claude Code at the broker, and the clone URL that aims git at it.
pub struct AgentGrant {
    pub clone_url: String,
    pub env: Vec<(String, String)>,
}

/// Where a run's repository access comes from.
///
/// An http(s) `GITHUB_CLONE_URL_BASE` — production — is always `Leased`,
/// even with no GitHub token configured, because the lease also carries the
/// run's Anthropic credit. `Direct` exists because a git smart-HTTP proxy
/// structurally cannot front a non-HTTP upstream: a `file://` mirror (the
/// integration tests, offline development) clones exactly what it names —
/// and when a lease issuer is wired, such a run still gets an
/// anthropic-only lease, so its API credit does not ride raw either.
#[derive(Clone, Debug)]
pub enum CloneSource {
    /// Clone (and, for the landing, push) straight to this URL.
    Direct(String),
    /// Mint a run lease bound to `owner/name` and go through the broker.
    Leased { repo: String },
}

/// Mints, extends and revokes leases. Cheap to clone; handed to the Scout and
/// Builder dispatchers.
#[derive(Clone)]
pub struct LeaseIssuer {
    store: Arc<Store>,
    advertise_host: String,
    port: u16,
}

impl std::fmt::Debug for LeaseIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseIssuer")
            .field("advertise_host", &self.advertise_host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl LeaseIssuer {
    pub fn new(store: Arc<Store>, advertise_host: String, port: u16) -> Self {
        Self {
            store,
            advertise_host,
            port,
        }
    }

    /// A run lease for a Scout session or a Builder build: `anthropic` +
    /// `git-read`, bound to `repo` (`owner/name`), expiring at the run budget
    /// plus [`LEASE_SLACK`].
    pub async fn grant_agent(
        &self,
        kind: SubjectKind,
        subject_id: &str,
        repo: &str,
        budget: Duration,
    ) -> Result<AgentGrant, StoreError> {
        let (token, hash) = mint_token();
        let now = Utc::now();
        let lease = Lease {
            id: Uuid::new_v4().to_string(),
            token_hash: hash,
            scopes: Scopes::AGENT,
            repo: Some(repo.to_string()),
            subject_kind: kind,
            subject_id: subject_id.to_string(),
            created_at: now,
            expires_at: now + chrono::Duration::from_std(budget + LEASE_SLACK).unwrap_or_default(),
            revoked_at: None,
        };
        self.store.insert_lease(&lease).await?;
        info!(
            lease = %lease.id,
            subject_kind = kind.as_str(),
            subject = subject_id,
            repo,
            expires_at = %lease.expires_at,
            "minted an agent lease"
        );
        Ok(AgentGrant {
            clone_url: vm_clone_url(&self.advertise_host, self.port, repo, &token),
            env: vec![
                ("ANTHROPIC_API_KEY".to_string(), token),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    anthropic_base_url(&self.advertise_host, self.port),
                ),
            ],
        })
    }

    /// The Anthropic half of a grant alone, for a run whose clone source is
    /// [`CloneSource::Direct`]: the broker cannot proxy a `file://` repo, but
    /// there is no reason the run's API credit should ride raw because of
    /// that.
    pub async fn grant_anthropic_env(
        &self,
        kind: SubjectKind,
        subject_id: &str,
        budget: Duration,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let (token, hash) = mint_token();
        let now = Utc::now();
        let lease = Lease {
            id: Uuid::new_v4().to_string(),
            token_hash: hash,
            scopes: Scopes::ANTHROPIC_ONLY,
            repo: None,
            subject_kind: kind,
            subject_id: subject_id.to_string(),
            created_at: now,
            expires_at: now + chrono::Duration::from_std(budget + LEASE_SLACK).unwrap_or_default(),
            revoked_at: None,
        };
        self.store.insert_lease(&lease).await?;
        info!(
            lease = %lease.id,
            subject_kind = kind.as_str(),
            subject = subject_id,
            "minted an anthropic-only lease"
        );
        Ok(vec![
            ("ANTHROPIC_API_KEY".to_string(), token),
            (
                "ANTHROPIC_BASE_URL".to_string(),
                anthropic_base_url(&self.advertise_host, self.port),
            ),
        ])
    }

    /// The server's own fetch+push window for landing `build_id`'s branch.
    /// Loopback URL, [`LAND_LEASE_TTL`] long, revoked by the caller when the
    /// landing concludes.
    pub async fn grant_land(&self, build_id: &str, repo: &str) -> Result<String, StoreError> {
        let (token, hash) = mint_token();
        let now = Utc::now();
        let lease = Lease {
            id: Uuid::new_v4().to_string(),
            token_hash: hash,
            scopes: Scopes::LAND,
            repo: Some(repo.to_string()),
            subject_kind: SubjectKind::Land,
            subject_id: build_id.to_string(),
            created_at: now,
            expires_at: now + chrono::Duration::from_std(LAND_LEASE_TTL).unwrap_or_default(),
            revoked_at: None,
        };
        self.store.insert_lease(&lease).await?;
        info!(lease = %lease.id, build_id, repo, "minted a landing lease");
        Ok(land_clone_url(self.port, repo, &token))
    }

    /// Move a subject's unrevoked leases to `expires_at` — the reattach path,
    /// which knows the resumed deadline but (correctly) never the token. Also
    /// resurrects a lease that expired while the server was down: the VM
    /// still holds that token, and it is the only one it will ever have.
    pub async fn extend(
        &self,
        kind: SubjectKind,
        subject_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let n = self
            .store
            .extend_leases_for_subject(kind, subject_id, expires_at)
            .await?;
        debug!(subject_kind = kind.as_str(), subject = subject_id, extended = n, %expires_at,
               "extended leases");
        Ok(())
    }

    /// Revoke a subject's leases, best-effort: conclusion tightening on top
    /// of expiry, so a failure here is logged and swallowed — the run's
    /// outcome must never be lost to lease hygiene.
    pub async fn revoke_best_effort(&self, kind: SubjectKind, subject_id: &str) {
        match self.store.revoke_leases_for_subject(kind, subject_id).await {
            Ok(0) => {}
            Ok(n) => debug!(
                subject_kind = kind.as_str(),
                subject = subject_id,
                revoked = n,
                "revoked leases"
            ),
            Err(e) => warn!(subject_kind = kind.as_str(), subject = subject_id, error = %e,
                            "could not revoke leases; expiry will close them"),
        }
    }
}

// ---------------------------------------------------------------------------
// The proxy
// ---------------------------------------------------------------------------

/// Broker listener configuration, resolved from the environment by
/// `run::Config`.
#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub port: u16,
    /// Must cover both audiences: the VM subnet (agent leases) *and*
    /// loopback, which is where the server's own landing lease points its
    /// `git push`. The default `0.0.0.0` covers both; a narrower bind has to
    /// keep loopback reachable.
    pub bind: String,
    pub advertise_host: String,
    pub anthropic_upstream: String,
    /// Reuses `GITHUB_CLONE_URL_BASE` — the broker forwards to the same place
    /// clone URLs used to point at.
    pub git_upstream: String,
}

struct BrokerInner {
    store: Arc<Store>,
    secrets: Secrets,
    http: reqwest::Client,
    anthropic_upstream: String,
    git_upstream: String,
}

/// Shared state behind the broker router.
#[derive(Clone)]
pub struct BrokerState(Arc<BrokerInner>);

impl BrokerState {
    pub fn new(store: Arc<Store>, secrets: Secrets, config: &BrokerConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No total timeout: an Anthropic SSE response legitimately runs
            // for many minutes. The connect timeout plus the client hanging
            // up bound the broker's exposure.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client construction is infallible with these options");
        Self(Arc::new(BrokerInner {
            store,
            secrets,
            http,
            anthropic_upstream: config.anthropic_upstream.trim_end_matches('/').to_string(),
            git_upstream: config.git_upstream.trim_end_matches('/').to_string(),
        }))
    }
}

/// Bind the broker listener. Split from [`serve`] exactly like the API's
/// `bind`/`serve_on`: a port clash must be a startup error, before any work
/// is resumed against leases nothing will be able to redeem.
pub async fn bind(config: &BrokerConfig) -> std::io::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind((config.bind.as_str(), config.port)).await?;
    info!(
        addr = %listener.local_addr()?,
        advertise = %config.advertise_host,
        "credential broker listening"
    );
    Ok(listener)
}

/// Serve the broker until `shutdown` flips, then stop accepting and sever
/// whatever is still open after [`CONNECTION_GRACE`] — same argument as the
/// API's `serve_on`: an SSE passthrough never closes on our schedule, and the
/// in-VM supervisors resume from a severed connection by design.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: BrokerState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut grace_shutdown = shutdown.clone();
    let graceful = async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    };
    let serve = axum::serve(listener, router(state))
        .with_graceful_shutdown(graceful)
        .into_future();
    tokio::pin!(serve);

    tokio::select! {
        biased;
        result = &mut serve => result,
        () = async {
            let _ = grace_shutdown.wait_for(|stop| *stop).await;
            tokio::time::sleep(CONNECTION_GRACE).await;
        } => {
            info!(
                grace_secs = CONNECTION_GRACE.as_secs(),
                "broker connections still open after shutdown; severing them"
            );
            Ok(())
        }
    }
}

/// The broker's routes. Public for tests: a router with no listener still
/// answers.
pub fn router(state: BrokerState) -> Router {
    Router::new()
        .route("/git/{owner}/{repo}/info/refs", get(git_info_refs))
        .route("/git/{owner}/{repo}/git-upload-pack", post(git_upload_pack))
        .route(
            "/git/{owner}/{repo}/git-receive-pack",
            post(git_receive_pack),
        )
        .route("/anthropic/{*path}", any(anthropic_proxy))
        .with_state(state)
}

fn deny(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = status;
    if status == StatusCode::UNAUTHORIZED {
        // git only volunteers credentials after a challenge when they are not
        // already in the URL; ours always are, but answer correctly anyway.
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"tasks-broker\""),
        );
    }
    response
}

impl BrokerState {
    /// The gate every route goes through: token → live lease → scope → repo.
    ///
    /// Failures are terse on the wire (a lease is the only audience) and
    /// quiet in the log at `debug` — an expired lease knocking is the
    /// ordinary end of every run, not an incident.
    async fn authorize(
        &self,
        token: Option<&str>,
        scope: Scope,
        repo: Option<&str>,
    ) -> Result<Lease, Response> {
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Err(deny(StatusCode::UNAUTHORIZED, "a lease is required"));
        };
        let lease = match self.0.store.lease_by_token_hash(&token_hash(token)).await {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                debug!("broker request with an unknown lease");
                return Err(deny(StatusCode::UNAUTHORIZED, "unknown lease"));
            }
            Err(e) => {
                warn!(error = %e, "lease lookup failed");
                return Err(deny(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "lease lookup failed",
                ));
            }
        };
        if !lease.is_live(Utc::now()) {
            debug!(lease = %lease.id, "expired or revoked lease presented");
            return Err(deny(StatusCode::UNAUTHORIZED, "lease expired or revoked"));
        }
        if !lease.scopes.allows(scope) {
            warn!(lease = %lease.id, ?scope, "lease presented outside its scope");
            return Err(deny(StatusCode::FORBIDDEN, "outside this lease's scope"));
        }
        if let Some(repo) = repo
            && lease.repo.as_deref() != Some(repo)
        {
            warn!(lease = %lease.id, repo, "lease presented for the wrong repo");
            return Err(deny(StatusCode::FORBIDDEN, "outside this lease's repo"));
        }
        Ok(lease)
    }
}

/// Hop-by-hop and transport-owned headers, dropped in both directions. The
/// bodies re-stream (chunked), so lengths are recomputed; `host` belongs to
/// each hop; `expect` would stall a stream on a 100-continue nobody relays.
const STRIPPED: &[HeaderName] = &[
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::HOST,
    header::CONTENT_LENGTH,
    header::EXPECT,
];

fn forwardable(name: &HeaderName) -> bool {
    !STRIPPED.contains(name) && name.as_str() != "keep-alive" && name.as_str() != "proxy-connection"
}

/// Copy client headers onto the upstream request, minus transport headers and
/// minus anything credential-shaped: the whole point is that the only
/// authorization upstream sees is the one this process injects.
fn upstream_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        if forwardable(name) && *name != header::AUTHORIZATION && name.as_str() != "x-api-key" {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Copy upstream response headers onto ours, minus transport headers.
fn response_headers(src: &HeaderMap, dst: &mut HeaderMap) {
    for (name, value) in src {
        if forwardable(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// Forward `body` to `url` with `method`, injecting `headers`, and stream the
/// answer back. Shared by both proxies: from here down the broker is a pipe.
async fn forward(
    state: &BrokerState,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // A GET/HEAD carries no body upstream: a streamed body has no length, so
    // reqwest would send `transfer-encoding: chunked`, and a chunked GET is
    // exactly the kind of request an upstream is allowed to refuse — git's
    // own `info/refs` handshake is a GET.
    let request = state.0.http.request(method.clone(), &url).headers(headers);
    let request = if matches!(method, Method::GET | Method::HEAD) {
        request
    } else {
        request.body(reqwest::Body::wrap_stream(body.into_data_stream()))
    };
    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            // The URL is credential-free by construction (auth rides a
            // header), so this cannot echo a secret.
            warn!(error = %e, url, "upstream request failed");
            return deny(StatusCode::BAD_GATEWAY, "upstream unreachable");
        }
    };
    let status = upstream.status();
    // Snapshot the headers before `bytes_stream` consumes the response.
    let mut headers_out = HeaderMap::new();
    response_headers(upstream.headers(), &mut headers_out);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers_out;
    response
}

/// The Basic-auth password, which is where git carries a URL userinfo
/// credential.
fn basic_password(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (_user, pass) = text.split_once(':')?;
    Some(pass.to_string())
}

/// The lease on an Anthropic-shaped request: `x-api-key` (what Claude Code
/// sends for an API key) or `Authorization: Bearer`.
fn api_key_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.to_string())
}

/// `owner/repo` out of the path pair, with the `.git` suffix git appends.
fn repo_key(owner: &str, repo: &str) -> String {
    format!("{}/{}", owner, repo.strip_suffix(".git").unwrap_or(repo))
}

/// The upstream Basic credential GitHub expects for a token:
/// `x-access-token:<token>`.
fn github_basic(token: &str) -> HeaderValue {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let mut value = HeaderValue::from_str(&format!("Basic {encoded}"))
        .expect("base64 is always a valid header value");
    value.set_sensitive(true);
    value
}

async fn git_proxy(
    state: BrokerState,
    owner: String,
    repo: String,
    tail: &str,
    query: Option<String>,
    scope: Scope,
    req: Request,
) -> Response {
    let repo_key = repo_key(&owner, &repo);
    let lease = match state
        .authorize(
            basic_password(req.headers()).as_deref(),
            scope,
            Some(&repo_key),
        )
        .await
    {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    debug!(lease = %lease.id, repo = %repo_key, tail, "git passthrough");

    let mut url = format!("{}/{}.git/{}", state.0.git_upstream, repo_key, tail);
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    let mut headers = upstream_headers(req.headers());
    // No token configured forwards anonymously — same reach as the old
    // uncredentialed clone URL: public repos work, private ones 401 upstream.
    if let Some(token) = state.0.secrets.github_token() {
        headers.insert(header::AUTHORIZATION, github_basic(token.expose()));
    }
    let method = req.method().clone();
    forward(&state, method, url, headers, req.into_body()).await
}

async fn git_info_refs(
    State(state): State<BrokerState>,
    Path((owner, repo)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    // The advertised service names which permission is being exercised:
    // `git-upload-pack` is a fetch, `git-receive-pack` is a push. Parsed as
    // an exact key=value pair, not a substring — a permission check must not
    // be foolable by a lookalike parameter.
    let service = query
        .as_deref()
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("service="));
    let scope = match service {
        Some("git-receive-pack") => Scope::GitWrite,
        Some("git-upload-pack") => Scope::GitRead,
        _ => {
            return deny(
                StatusCode::BAD_REQUEST,
                "smart HTTP only (service=git-upload-pack|git-receive-pack)",
            );
        }
    };
    git_proxy(state, owner, repo, "info/refs", query, scope, req).await
}

async fn git_upload_pack(
    State(state): State<BrokerState>,
    Path((owner, repo)): Path<(String, String)>,
    req: Request,
) -> Response {
    git_proxy(
        state,
        owner,
        repo,
        "git-upload-pack",
        None,
        Scope::GitRead,
        req,
    )
    .await
}

async fn git_receive_pack(
    State(state): State<BrokerState>,
    Path((owner, repo)): Path<(String, String)>,
    req: Request,
) -> Response {
    git_proxy(
        state,
        owner,
        repo,
        "git-receive-pack",
        None,
        Scope::GitWrite,
        req,
    )
    .await
}

async fn anthropic_proxy(
    State(state): State<BrokerState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let lease = match state
        .authorize(
            api_key_token(req.headers()).as_deref(),
            Scope::Anthropic,
            None,
        )
        .await
    {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    let Some(key) = state.0.secrets.anthropic_api_key() else {
        warn!(lease = %lease.id, "anthropic request with no key configured on the host");
        return deny(
            StatusCode::BAD_GATEWAY,
            "the host has no anthropic key configured (`tasks secrets set anthropic-api-key`)",
        );
    };
    debug!(lease = %lease.id, path, "anthropic passthrough");

    let mut url = format!("{}/{}", state.0.anthropic_upstream, path);
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    let mut headers = upstream_headers(req.headers());
    let mut key_value =
        HeaderValue::from_str(key.expose()).unwrap_or_else(|_| HeaderValue::from_static(""));
    key_value.set_sensitive(true);
    headers.insert("x-api-key", key_value);
    let method = req.method().clone();
    forward(&state, method, url, headers, req.into_body()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_round_trip_their_encoding() {
        for scopes in [Scopes::AGENT, Scopes::LAND] {
            assert_eq!(Scopes::decode(&scopes.encode()), scopes);
        }
        // Unknown words are ignored, known ones kept.
        assert_eq!(
            Scopes::decode("anthropic warp-drive git-read"),
            Scopes {
                anthropic: true,
                git_read: true,
                git_write: false
            }
        );
    }

    #[test]
    fn agent_scopes_cannot_push() {
        assert!(Scopes::AGENT.allows(Scope::GitRead));
        assert!(Scopes::AGENT.allows(Scope::Anthropic));
        assert!(!Scopes::AGENT.allows(Scope::GitWrite));
        assert!(!Scopes::LAND.allows(Scope::Anthropic));
    }

    #[test]
    fn minted_tokens_are_prefixed_and_hash_deterministically() {
        let (token, hash) = mint_token();
        assert!(token.starts_with("tl-"));
        assert_eq!(hash, token_hash(&token));
        let (other, _) = mint_token();
        assert_ne!(token, other);
    }

    #[test]
    fn urls_place_the_lease_in_userinfo_only() {
        let url = vm_clone_url("192.168.64.1", 4801, "o/r", "tl-abc");
        assert_eq!(
            url,
            "http://x-access-token:tl-abc@192.168.64.1:4801/git/o/r.git"
        );
        // The existing redaction layers scrub exactly this shape.
        assert_eq!(
            tasks_protocol::redact::redact(&url),
            "http://***@192.168.64.1:4801/git/o/r.git"
        );
        assert_eq!(
            anthropic_base_url("192.168.64.1", 4801),
            "http://192.168.64.1:4801/anthropic"
        );
    }

    #[test]
    fn repo_keys_strip_the_git_suffix() {
        assert_eq!(repo_key("o", "r.git"), "o/r");
        assert_eq!(repo_key("o", "r"), "o/r");
    }

    #[test]
    fn basic_password_reads_the_lease_out_of_git_credentials() {
        let mut headers = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("x-access-token:tl-lease");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(basic_password(&headers).as_deref(), Some("tl-lease"));
        assert_eq!(api_key_token(&HeaderMap::new()), None);
    }
}
