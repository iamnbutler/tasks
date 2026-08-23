//! The credential broker end to end over HTTP.
//!
//! The unit tests in `broker.rs` cover encodings and URL shapes. This file
//! asks the only question that matters operationally: with a real listener, a
//! real store and real leases, **does the raw credential go up and does the
//! lease stay down** — and is a lease refused everything it was not minted
//! for?
//!
//! Both upstreams are one real axum server on loopback that records what it
//! was asked, in the idiom `tests/custodial.rs` uses for GitHub. That keeps
//! the assertions on the wire rather than on our own bookkeeping: the whole
//! claim of #971 is about what a request *contains* by the time it leaves
//! this process.
//!
//! The scope tests are the load-bearing ones. "A Scout or Builder cannot push
//! whatever its prompt talks it into" is a sentence in `crates/tasks/src/broker.rs`;
//! here it is a 403 with an untouched upstream behind it.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use tasks::broker::{self, BrokerConfig, BrokerState, LeaseIssuer, SubjectKind};
use tasks::secrets::Secrets;
use tasks::store::Store;
use tokio::sync::watch;

/// The real credentials the host holds. Deliberately distinctive strings: a
/// test that asserts "the lease did not go upstream" is only honest if the
/// two values cannot be confused for one another anywhere in a header.
const REAL_ANTHROPIC_KEY: &str = "sk-ant-the-hosts-real-key";
const REAL_GITHUB_TOKEN: &str = "ghp_the_hosts_real_token";

const REPO: &str = "acme/widgets";

/// One request as the upstream saw it.
#[derive(Clone, Debug)]
struct SeenRequest {
    uri: String,
    authorization: Option<String>,
    api_key: Option<String>,
    /// A header that is neither transport nor credential, kept so a test can
    /// assert the broker is a passthrough rather than only a gate.
    anthropic_version: Option<String>,
    body: String,
}

impl SeenRequest {
    /// Every header this request could have carried a credential in, joined —
    /// so an assertion about a secret's absence covers the ones the next
    /// route adds, not only the one this test was written against.
    fn credential_surface(&self) -> String {
        format!(
            "{} {} {} {}",
            self.uri,
            self.authorization.clone().unwrap_or_default(),
            self.api_key.clone().unwrap_or_default(),
            self.body
        )
    }
}

/// A single upstream standing in for both api.anthropic.com and github.com:
/// the broker addresses them by URL, so one recorder answers both and a test
/// asserting "nothing reached upstream" means it about every route at once.
async fn spawn_upstream() -> (String, Arc<Mutex<Vec<SeenRequest>>>) {
    let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .fallback(record)
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

async fn record(
    State(seen): State<Arc<Mutex<Vec<SeenRequest>>>>,
    _method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    seen.lock().unwrap().push(SeenRequest {
        uri: uri.to_string(),
        authorization: header("authorization"),
        api_key: header("x-api-key"),
        anthropic_version: header("anthropic-version"),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    (StatusCode::OK, "upstream answered".to_string())
}

struct Harness {
    leases: LeaseIssuer,
    store: Arc<Store>,
    /// `http://127.0.0.1:<broker port>`, as a VM would address it.
    broker: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    http: reqwest::Client,
    /// Held so the broker's shutdown channel stays open for the test's life.
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    fn upstream_requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// The one assertion every refusal test makes: a rejected request must not
    /// have been forwarded. A 403 that still reached GitHub would be a
    /// reporting bug hiding a real one.
    fn assert_upstream_untouched(&self) {
        let seen = self.upstream_requests();
        assert!(
            seen.is_empty(),
            "upstream should not have been called: {seen:?}"
        );
    }

    fn info_refs(&self, repo: &str, service: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!(
                "{}/git/{repo}/info/refs?service={service}",
                self.broker
            ))
            .basic_auth("x-access-token", Some(token))
    }
}

async fn harness() -> Harness {
    let (upstream, seen) = spawn_upstream().await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let secrets = Secrets::for_tests(Some(REAL_GITHUB_TOKEN), Some(REAL_ANTHROPIC_KEY));

    // Port 0, then read the bound port back: the advertised address has to be
    // the one the listener actually got, and `LeaseIssuer` builds URLs from it.
    let config = BrokerConfig {
        port: 0,
        bind: "127.0.0.1".to_string(),
        advertise_host: "127.0.0.1".to_string(),
        anthropic_upstream: upstream.clone(),
        git_upstream: upstream,
    };
    let listener = broker::bind(&config).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let state = BrokerState::new(store.clone(), secrets, &config);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        broker::serve(listener, state, shutdown_rx).await.unwrap();
    });

    Harness {
        leases: LeaseIssuer::new(store.clone(), "127.0.0.1".to_string(), port),
        store,
        broker: format!("http://127.0.0.1:{port}"),
        seen,
        http: reqwest::Client::new(),
        _shutdown: shutdown_tx,
    }
}

/// The lease a dispatched Scout would hold, read out of the env the VM gets —
/// so these tests exercise the same string the supervisor would.
async fn agent_lease(h: &Harness, session: &str) -> String {
    let grant = h
        .leases
        .grant_agent(
            SubjectKind::Scout,
            session,
            REPO,
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    let token = grant
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_API_KEY")
        .map(|(_, v)| v.clone())
        .expect("a grant carries the lease as ANTHROPIC_API_KEY");
    assert!(
        grant.clone_url.contains(&token),
        "the clone URL carries the same lease"
    );
    token
}

/// The central claim: the VM authenticates with a lease, the upstream sees the
/// host's real key, and the two never swap places.
#[tokio::test]
async fn an_agent_lease_is_exchanged_for_the_hosts_key_and_never_forwarded() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-1").await;

    let response = h
        .http
        .post(format!("{}/anthropic/v1/messages", h.broker))
        .header("x-api-key", &lease)
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-opus-5"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = h.upstream_requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let request = &seen[0];
    assert_eq!(request.api_key.as_deref(), Some(REAL_ANTHROPIC_KEY));
    assert!(
        !request.credential_surface().contains(&lease),
        "the lease must not reach the upstream: {request:?}"
    );
    // The path, body and ordinary headers are a passthrough, not a rewrite:
    // the broker replaces the credential and nothing else.
    assert!(request.uri.starts_with("/v1/messages"), "{request:?}");
    assert!(request.body.contains("claude-opus-5"), "{request:?}");
    assert_eq!(request.anthropic_version.as_deref(), Some("2023-06-01"));
}

/// A fetch is what an agent lease is for, and the upstream sees GitHub's own
/// Basic shape with the real token in it.
#[tokio::test]
async fn an_agent_lease_fetches_with_the_hosts_token_injected() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-2").await;

    let response = h
        .info_refs(REPO, "git-upload-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = h.upstream_requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let request = &seen[0];
    let expected = format!(
        "Basic {}",
        base64_standard(&format!("x-access-token:{REAL_GITHUB_TOKEN}"))
    );
    assert_eq!(request.authorization.as_deref(), Some(expected.as_str()));
    assert!(
        !request.credential_surface().contains(&lease),
        "the lease must not reach the upstream: {request:?}"
    );
    assert!(
        request.uri.contains("acme/widgets.git/info/refs"),
        "{request:?}"
    );
}

/// The scope rule: "a Scout or Builder cannot push whatever its prompt talks it
/// into". Both halves of a smart-HTTP push are refused, and neither reaches
/// GitHub — the scope is enforcement, not description.
#[tokio::test]
async fn an_agent_lease_cannot_push() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-3").await;

    let advertise = h
        .info_refs(REPO, "git-receive-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(advertise.status(), StatusCode::FORBIDDEN);

    // The advertisement is only the handshake; the POST that carries the
    // objects is refused on its own, so a client that skips the handshake
    // gains nothing.
    let push = h
        .http
        .post(format!("{}/git/{REPO}/git-receive-pack", h.broker))
        .basic_auth("x-access-token", Some(&lease))
        .body("0000")
        .send()
        .await
        .unwrap();
    assert_eq!(push.status(), StatusCode::FORBIDDEN);

    h.assert_upstream_untouched();
}

/// The server's own landing lease is the only thing that may push — which is
/// what keeps the PAT out of host-side `git` argv as well as out of VMs.
#[tokio::test]
async fn only_a_landing_lease_may_push() {
    let h = harness().await;
    let url = h.leases.grant_land("build-1", REPO).await.unwrap();
    // Loopback, because this lease never leaves the host.
    assert!(url.starts_with("http://x-access-token:"), "{url}");
    assert!(url.contains("@127.0.0.1:"), "{url}");
    let lease = url
        .split_once("x-access-token:")
        .and_then(|(_, rest)| rest.split_once('@'))
        .map(|(token, _)| token.to_string())
        .expect("the landing URL carries its lease in the userinfo");

    let response = h
        .info_refs(REPO, "git-receive-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = h.upstream_requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0]
            .authorization
            .as_deref()
            .is_some_and(|a| a.contains(&base64_standard(&format!(
                "x-access-token:{REAL_GITHUB_TOKEN}"
            )))),
        "{:?}",
        seen[0]
    );
}

/// A lease is bound to the repo it was minted for. Without this, one run's
/// lease reads every repository the host's token can.
#[tokio::test]
async fn a_lease_is_refused_outside_its_repo() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-4").await;

    let response = h
        .info_refs("acme/other", "git-upload-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    h.assert_upstream_untouched();
}

/// The `.git` suffix git appends is part of the URL, not part of the repo, so
/// the binding has to survive it — a real clone sends it.
#[tokio::test]
async fn the_dot_git_suffix_does_not_break_the_repo_binding() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-5").await;

    let response = h
        .info_refs("acme/widgets.git", "git-upload-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Revocation is what a concluded run does; expiry is the backstop behind it.
/// Both end at the same place, and neither reaches upstream.
#[tokio::test]
async fn a_revoked_lease_is_refused() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-6").await;

    // It works first, so the refusal below is the revocation and not a
    // mis-built request.
    assert_eq!(
        h.info_refs(REPO, "git-upload-pack", &lease)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    h.seen.lock().unwrap().clear();

    h.store
        .revoke_leases_for_subject(SubjectKind::Scout, "session-6")
        .await
        .unwrap();

    let response = h
        .info_refs(REPO, "git-upload-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let anthropic = h
        .http
        .post(format!("{}/anthropic/v1/messages", h.broker))
        .header("x-api-key", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(anthropic.status(), StatusCode::UNAUTHORIZED);

    h.assert_upstream_untouched();
}

/// An expired lease is refused without anything having to revoke it — the
/// property that makes a leaked token bounded rather than permanent.
#[tokio::test]
async fn an_expired_lease_is_refused() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-7").await;

    h.leases
        .extend(
            SubjectKind::Scout,
            "session-7",
            chrono::Utc::now() - chrono::Duration::seconds(1),
        )
        .await
        .unwrap();

    let response = h
        .info_refs(REPO, "git-upload-pack", &lease)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    h.assert_upstream_untouched();
}

/// The broker is not an open proxy: no lease and an unknown lease are both
/// refused, and neither spends the host's credentials.
#[tokio::test]
async fn an_absent_or_unknown_lease_never_spends_the_hosts_credentials() {
    let h = harness().await;

    let anonymous = h
        .http
        .get(format!(
            "{}/git/{REPO}/info/refs?service=git-upload-pack",
            h.broker
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let invented = h
        .info_refs(REPO, "git-upload-pack", "not-a-lease-anyone-minted")
        .send()
        .await
        .unwrap();
    assert_eq!(invented.status(), StatusCode::UNAUTHORIZED);

    let anthropic = h
        .http
        .post(format!("{}/anthropic/v1/messages", h.broker))
        .send()
        .await
        .unwrap();
    assert_eq!(anthropic.status(), StatusCode::UNAUTHORIZED);

    h.assert_upstream_untouched();
}

/// A permission is decided by the advertised service, parsed as a whole
/// value. A lookalike parameter must not read as a fetch and let a push
/// through the handshake.
#[tokio::test]
async fn a_lookalike_service_parameter_is_not_a_permission() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-8").await;

    for query in [
        "service=git-receive-pack-not-really",
        "notservice=git-upload-pack",
        "",
    ] {
        let response = h
            .http
            .get(format!("{}/git/{REPO}/info/refs?{query}", h.broker))
            .basic_auth("x-access-token", Some(&lease))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "query {query:?} should not name a permission"
        );
    }
    h.assert_upstream_untouched();
}

/// A client's own `authorization` / `x-api-key` is replaced, never merged:
/// the only credential upstream sees is the one this process injects.
#[tokio::test]
async fn a_clients_own_credential_headers_are_stripped() {
    let h = harness().await;
    let lease = agent_lease(&h, "session-9").await;

    h.http
        .post(format!("{}/anthropic/v1/messages", h.broker))
        .header("x-api-key", &lease)
        .header("authorization", "Bearer sk-ant-someone-elses-key")
        .send()
        .await
        .unwrap();

    let seen = h.upstream_requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].api_key.as_deref(), Some(REAL_ANTHROPIC_KEY));
    assert!(
        !seen[0].credential_surface().contains("someone-elses-key"),
        "{:?}",
        seen[0]
    );
}

fn base64_standard(value: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(value)
}
