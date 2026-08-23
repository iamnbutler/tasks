//! The GitHub device flow (#1002), driven against a real local HTTP server
//! standing in for GitHub's OAuth endpoints — the `tests/broker.rs` idiom,
//! never a mock of our own client. The fake answers `/login/device/code` with
//! a fixed grant whose `interval` is 0 (so the poll loop spins without
//! wall-clock cost) and `/login/oauth/access_token` with the next scripted
//! answer, which is how each test states the exact conversation it is about.
//!
//! One test execs the `tasks` binary (`auth login`, end to end into the
//! sealed store). Per the `Command::env_remove`-is-not-a-scrub rule, any test
//! that execs the binary sets `TASKS_ENV_FILES=off` so a developer's
//! untracked `.env` cannot decide its behaviour.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use tasks::auth;
use tasks::secrets::{self, SecretName};

type Script = Arc<Mutex<VecDeque<Value>>>;

const FAKE_TOKEN: &str = "gho_fake_device_flow_token_for_tests";

async fn device_code(_state: State<Script>) -> Json<Value> {
    Json(json!({
        "device_code": "dc_fake",
        "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 0,
    }))
}

async fn access_token(State(script): State<Script>) -> Json<Value> {
    Json(
        script
            .lock()
            .expect("script lock")
            .pop_front()
            .expect("the flow polled more times than the test scripted"),
    )
}

/// Bind the fake on 127.0.0.1:0 and return its base URL.
async fn fake_github(scripted: Vec<Value>) -> String {
    let script: Script = Arc::new(Mutex::new(scripted.into()));
    let app = Router::new()
        .route("/login/device/code", post(device_code))
        .route("/login/oauth/access_token", post(access_token))
        .with_state(script);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake github");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve fake github");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_pending_then_slowed_poll_reaches_the_token() {
    let base = fake_github(vec![
        json!({"error": "authorization_pending"}),
        json!({"error": "slow_down", "interval": 0}),
        json!({"access_token": FAKE_TOKEN, "token_type": "bearer", "scope": "repo workflow"}),
    ])
    .await;

    let authorization = auth::request_code(&base).await.expect("request_code");
    assert_eq!(authorization.user_code, "ABCD-1234");
    assert_eq!(
        authorization.verification_uri,
        "https://github.com/login/device"
    );

    let token = auth::poll_for_token(&base, &authorization)
        .await
        .expect("poll_for_token");
    assert_eq!(token.expose(), FAKE_TOKEN);
}

/// The #1002 decision, enforced rather than trusted: a grant that arrives
/// expiring means the OAuth app's "Expire user access tokens" setting is on,
/// and sealing it would plant a credential that dies mid-build. The error
/// names the checkbox, because the human reading it is one settings page away
/// from the fix.
#[tokio::test]
async fn an_expiring_token_is_refused_and_names_the_checkbox() {
    let base = fake_github(vec![json!({
        "access_token": FAKE_TOKEN,
        "token_type": "bearer",
        "expires_in": 28800,
        "refresh_token": "ghr_fake_refresh",
    })])
    .await;

    let authorization = auth::request_code(&base).await.expect("request_code");
    let err = auth::poll_for_token(&base, &authorization)
        .await
        .expect_err("an expiring token must be refused");
    let message = err.to_string();
    assert!(
        message.contains("Expire user access tokens"),
        "the refusal must name the setting to uncheck; said: {message}"
    );
    assert!(
        message.contains("Nothing was stored"),
        "the refusal must say nothing was sealed; said: {message}"
    );
}

#[tokio::test]
async fn a_denied_authorization_is_terminal() {
    let base = fake_github(vec![json!({"error": "access_denied"})]).await;
    let authorization = auth::request_code(&base).await.expect("request_code");
    let err = auth::poll_for_token(&base, &authorization)
        .await
        .expect_err("access_denied is terminal");
    assert!(err.to_string().contains("declined"), "said: {err}");
}

#[tokio::test]
async fn an_expired_code_says_to_start_again() {
    let base = fake_github(vec![json!({"error": "expired_token"})]).await;
    let authorization = auth::request_code(&base).await.expect("request_code");
    let err = auth::poll_for_token(&base, &authorization)
        .await
        .expect_err("expired_token is terminal");
    // Surface-neutral on purpose (#1061): the same sentence reaches the CLI's
    // stderr and the pane's Refused state, and each surface's remedy is its
    // own way of starting again.
    assert!(
        err.to_string().contains("start again"),
        "the expiry must name the act that recovers; said: {err}"
    );
}

/// GitHub's other refusals (`device_flow_disabled`,
/// `incorrect_client_credentials`, …) are reported in GitHub's own
/// vocabulary, so the message stays diagnosable without this module keeping
/// a translation table that goes stale.
#[tokio::test]
async fn an_unknown_refusal_reports_githubs_own_words() {
    let base = fake_github(vec![json!({
        "error": "device_flow_disabled",
        "error_description": "Device flow is not enabled for this app",
    })])
    .await;
    let authorization = auth::request_code(&base).await.expect("request_code");
    let err = auth::poll_for_token(&base, &authorization)
        .await
        .expect_err("device_flow_disabled is terminal");
    let message = err.to_string();
    assert!(message.contains("device_flow_disabled"), "said: {message}");
    assert!(
        message.contains("Device flow is not enabled"),
        "the description must ride along; said: {message}"
    );
}

/// End to end through the binary: `tasks auth login` seals the token exactly
/// where `tasks secrets set github-token` would, and the sealed value
/// round-trips — asserted by opening the store in-process, not by trusting
/// the CLI's own success line.
#[tokio::test]
async fn the_cli_seals_the_token_where_secrets_set_does() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let key_file = dir.path().join("unseal.key");
    secrets::init(&data_dir, Some(&key_file)).expect("init sealed store");

    let base = fake_github(vec![json!({
        "access_token": FAKE_TOKEN,
        "token_type": "bearer",
        "scope": "repo workflow",
    })])
    .await;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tasks"))
        .args(["auth", "login"])
        .env("TASKS_ENV_FILES", "off")
        .env("TASKS_DATA_DIR", &data_dir)
        .env("GITHUB_OAUTH_URL", &base)
        .output()
        .await
        .expect("run tasks auth login");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "auth login failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("ABCD-1234"),
        "the user code must be shown; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("sealed `github-token`"),
        "stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains(FAKE_TOKEN) && !stderr.contains(FAKE_TOKEN),
        "the token must never be printed"
    );

    let sealed = secrets::Secrets::open(&data_dir)
        .expect("open sealed store")
        .get(SecretName::GithubToken)
        .expect("github-token sealed");
    assert_eq!(sealed.expose(), FAKE_TOKEN);
}

/// The store probe runs before GitHub is involved: with no sealed store the
/// command refuses immediately, names `tasks secrets init`, and never prints
/// a code for the human to walk through pointlessly.
#[tokio::test]
async fn a_missing_store_refuses_before_showing_a_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    // No fake server at all: the refusal must come before any HTTP.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tasks"))
        .args(["auth", "login"])
        .env("TASKS_ENV_FILES", "off")
        .env("TASKS_DATA_DIR", &data_dir)
        .env("GITHUB_OAUTH_URL", "http://127.0.0.1:1")
        .output()
        .await
        .expect("run tasks auth login");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "must refuse with no store");
    assert!(
        stderr.contains("tasks secrets init"),
        "the refusal must name the fix\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("enter"),
        "no code may be shown before the store exists; stdout:\n{stdout}"
    );
}

// --- the routes the app drives (#1061) --------------------------------------

use std::time::Duration;

use tasks::store::Store;
use tasks_api::http::{DeviceFlow, DeviceFlowStatus};

/// A real server whose device-flow service points at the fake GitHub, sealing
/// into a real sealed store under a tempdir — the same store the assertion
/// reads back through `Secrets::open`.
struct SignInHarness {
    base: String,
    store: Arc<Store>,
    data_dir: std::path::PathBuf,
    http: reqwest::Client,
    /// Keeps the tempdir (and the sealed store in it) alive for the test.
    _dir: tempfile::TempDir,
}

async fn sign_in_harness(scripted: Vec<Value>) -> SignInHarness {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    secrets::init(&data_dir, Some(&dir.path().join("unseal.key"))).expect("init sealed store");

    let oauth_base = fake_github(scripted).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let flows = Arc::new(tasks::auth::DeviceFlows::new(
        data_dir.clone(),
        secrets::Secrets::open(&data_dir).expect("open secrets"),
        oauth_base,
    ));
    let app = tasks::server::router_with_services(
        store.clone(),
        tasks::server::Services {
            device_auth: Some(flows),
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    SignInHarness {
        base,
        store,
        data_dir,
        http: reqwest::Client::new(),
        _dir: dir,
    }
}

/// Poll `GET /auth/github/device` until the flow leaves `Pending`, bounded so
/// a hang is a test failure rather than a stuck suite.
async fn wait_for_conclusion(h: &SignInHarness) -> DeviceFlowStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status: DeviceFlowStatus = h
            .http
            .get(format!("{}/auth/github/device", h.base))
            .send()
            .await
            .expect("GET device flow")
            .json()
            .await
            .expect("decode device flow status");
        match status.flow {
            DeviceFlow::Pending { .. } | DeviceFlow::Idle => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the flow never concluded"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            _ => return status,
        }
    }
}

#[tokio::test]
async fn the_pane_flow_starts_polls_and_seals_without_the_token_transiting() {
    let h = sign_in_harness(vec![
        json!({"error": "authorization_pending"}),
        json!({"access_token": FAKE_TOKEN, "token_type": "bearer", "scope": "repo workflow"}),
    ])
    .await;

    // The POST answers already-Pending, code included: one round trip gives
    // the pane everything it shows.
    let response = h
        .http
        .post(format!("{}/auth/github/device", h.base))
        .send()
        .await
        .expect("POST device flow");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(
        body.contains("ABCD-1234"),
        "the pending answer carries the code; got: {body}"
    );

    let concluded = wait_for_conclusion(&h).await;
    let DeviceFlow::Sealed { .. } = concluded.flow else {
        panic!("expected sealed, got {:?}", concluded.flow);
    };
    // The credential line flips to the sealed store, which is the observation
    // the app's empty states hand to this route.
    assert_eq!(concluded.credential.as_deref(), Some("the sealed store"));

    // The token itself round-trips into the store the CLI writes...
    let sealed = secrets::Secrets::open(&h.data_dir)
        .expect("open sealed store")
        .get(SecretName::GithubToken)
        .expect("github-token sealed");
    assert_eq!(sealed.expose(), FAKE_TOKEN);

    // ...and never appears in anything the API answered. Asserted on the raw
    // body rather than the decoded struct, so a field added later cannot
    // smuggle it past the test.
    let raw = h
        .http
        .get(format!("{}/auth/github/device", h.base))
        .send()
        .await
        .expect("GET device flow")
        .text()
        .await
        .expect("raw body");
    assert!(
        !raw.contains(FAKE_TOKEN) && !body.contains(FAKE_TOKEN),
        "the token must never transit the API"
    );
}

#[tokio::test]
async fn an_agent_is_refused_and_no_flow_starts() {
    let h = sign_in_harness(vec![]).await;

    let actor = format!("orchestrator {}", h.store.actor_token().expose());
    let refused = h
        .http
        .post(format!("{}/auth/github/device", h.base))
        .header("X-Tasks-Actor", &actor)
        .send()
        .await
        .expect("POST as orchestrator");
    assert_eq!(refused.status(), 403);
    let read_refused = h
        .http
        .get(format!("{}/auth/github/device", h.base))
        .header("X-Tasks-Actor", &actor)
        .send()
        .await
        .expect("GET as orchestrator");
    assert_eq!(
        read_refused.status(),
        403,
        "the pending state carries the user code, which decides whose account the pipeline becomes"
    );

    // The human's view proves the refusal was a no-op: nothing started, and
    // the scripted queue being empty proves GitHub was never asked (the fake
    // panics on an unscripted poll).
    let status: DeviceFlowStatus = h
        .http
        .get(format!("{}/auth/github/device", h.base))
        .send()
        .await
        .expect("GET as human")
        .json()
        .await
        .expect("decode");
    assert_eq!(status.flow, DeviceFlow::Idle);
}

#[tokio::test]
async fn a_refusal_reaches_the_pane_verbatim() {
    let h = sign_in_harness(vec![json!({
        "access_token": FAKE_TOKEN,
        "token_type": "bearer",
        "expires_in": 28800,
        "refresh_token": "ghr_fake_refresh",
    })])
    .await;

    let response = h
        .http
        .post(format!("{}/auth/github/device", h.base))
        .send()
        .await
        .expect("POST device flow");
    assert_eq!(response.status(), 200);

    let concluded = wait_for_conclusion(&h).await;
    let DeviceFlow::Refused { message, .. } = concluded.flow else {
        panic!("expected refused, got {:?}", concluded.flow);
    };
    // The sentence was written for the human at the pane: it names the OAuth
    // app checkbox to uncheck, and nothing between here and the pane may
    // rewrite it.
    assert!(
        message.contains("Expire user access tokens"),
        "said: {message}"
    );

    // And nothing was sealed.
    assert!(
        secrets::Secrets::open(&h.data_dir)
            .expect("open sealed store")
            .get(SecretName::GithubToken)
            .is_none(),
        "a refused flow must seal nothing"
    );
}

#[tokio::test]
async fn a_router_without_the_service_answers_503_never_a_guess() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router_with_services(store, tasks::server::Services::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let response = reqwest::Client::new()
        .get(format!("{base}/auth/github/device"))
        .send()
        .await
        .expect("GET device flow");
    assert_eq!(
        response.status(),
        503,
        "a router with no credential store has nothing true to say about the credential"
    );
}
