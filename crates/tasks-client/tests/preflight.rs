//! The connect-time build check, against real servers on real ports.
//!
//! Repo convention, and the point of the test: the "server" here is the actual
//! `tasks::server::router` in the case that matters, so a change to the route
//! or its body shape fails here rather than in a UI six months later.
//!
//! Threading follows `tests/client.rs`: the async server runs on a runtime the
//! test holds, and the blocking client calls it from the test thread exactly
//! as a GUI worker thread would.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use tasks_api::version::VersionInfo;
use tasks_client::{Client, ClientError, Preflight};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Serve `app` on an OS-assigned loopback port; returns its address.
fn spawn(runtime: &tokio::runtime::Runtime, app: Router) -> SocketAddr {
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    })
}

fn client(addr: SocketAddr) -> Client {
    Client::with_base(format!("http://{addr}"))
}

/// Both ends ship from one tree, so the real router and this client must
/// agree — if they ever don't, that is the bug the whole feature is about.
#[test]
fn real_server_says_this_client_is_current() {
    let runtime = runtime();
    let store =
        runtime.block_on(async { Arc::new(tasks::store::Store::open_in_memory().await.unwrap()) });
    let addr = spawn(&runtime, tasks::server::router(store));

    let client = client(addr);
    let info = client.server_version().unwrap();
    assert_eq!(info.version, tasks::version::VERSION);

    let verdict = client.preflight().unwrap();
    assert!(
        matches!(verdict, Preflight::Current { .. }),
        "expected Current, got {verdict:?}"
    );
    assert_eq!(verdict.warning(), None);
}

/// A floor no client in this tree can meet — the shape of the day someone
/// actually breaks the wire.
#[test]
fn client_under_the_floor_is_outdated_and_says_so() {
    let runtime = runtime();
    let addr = spawn(
        &runtime,
        Router::new().route(
            "/version",
            get(|| async {
                Json(VersionInfo {
                    version: "0.1.163".into(),
                    commit: "abc1234".into(),
                    min_client_version: "0.1.140".into(),
                })
            }),
        ),
    );

    let verdict = client(addr)
        .with_client_version("0.1.120")
        .preflight()
        .unwrap();
    assert!(verdict.is_outdated(), "got {verdict:?}");
    let warning = verdict.warning().expect("a stale client gets a line");
    assert!(warning.contains("0.1.120"), "{warning}");
    assert!(warning.contains("0.1.140"), "{warning}");
    assert!(warning.contains("0.1.163"), "{warning}");
}

/// A server without the route predates it, so *it* is the stale one. 404 is a
/// verdict, not an error.
#[test]
fn server_without_the_route_is_a_verdict() {
    let runtime = runtime();
    let addr = spawn(&runtime, Router::new());

    let verdict = client(addr)
        .with_client_version("0.1.163")
        .preflight()
        .unwrap();
    assert_eq!(
        verdict,
        Preflight::ServerUnversioned {
            client: "0.1.163".into()
        }
    );
    assert!(verdict.warning().is_some());
}

/// Nothing listening is the caller's existing "can't reach the server" case,
/// and stays an error rather than being folded into a version verdict.
#[test]
fn unreachable_server_is_still_an_error() {
    let runtime = runtime();
    // Bind and drop: the port was real a moment ago and is now closed, which
    // is the closest thing to a guaranteed-dead port.
    let addr = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    });

    let error = client(addr).preflight().unwrap_err();
    assert!(
        matches!(error, ClientError::Transport(_)),
        "expected a transport error, got {error:?}"
    );
}
