//! `GET /version`, over real HTTP.
//!
//! The route exists so a client can say "your app is old" instead of failing
//! as a random decode error, and so a restart can ask "is the new process up,
//! and is it the build I just made?" — both of which mean it has to answer
//! early, cheaply, and without touching the store.

use std::sync::Arc;

use serde_json::Value;
use tasks::store::Store;
use tasks_api::version::VersionInfo;

async fn serve() -> String {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

#[tokio::test]
async fn serves_this_build_identity() {
    let base = serve().await;
    let response = reqwest::get(format!("{base}/version")).await.unwrap();
    assert_eq!(response.status(), 200);

    let info: VersionInfo = response.json().await.unwrap();
    assert_eq!(info.version, tasks::version::VERSION);
    assert_eq!(info.commit, tasks::version::COMMIT);
    assert_eq!(info.min_client_version, tasks::version::MIN_CLIENT_VERSION);
}

/// Three flat strings, no nesting: `curl -s …/version | jq -r .version` is a
/// legitimate client of this route (a `make` target checking a restart is the
/// intended one), and nesting would break that for no gain.
#[tokio::test]
async fn body_is_three_flat_strings() {
    let base = serve().await;
    let body: Value = reqwest::get(format!("{base}/version"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let object = body.as_object().expect("an object");
    assert_eq!(object.len(), 3, "unexpected fields: {object:?}");
    for key in ["version", "commit", "min_client_version"] {
        let value = object.get(key).unwrap_or_else(|| panic!("missing {key}"));
        assert!(
            value.as_str().is_some_and(|s| !s.is_empty()),
            "{key} should be a non-empty string, got {value}"
        );
    }
}

/// The handler takes no `State`, so a router built over a store that is never
/// touched still answers — which is what makes it usable as the liveness poll
/// during a restart, before the database is open.
#[tokio::test]
async fn answers_without_reading_the_store() {
    let base = serve().await;
    for _ in 0..3 {
        let response = reqwest::get(format!("{base}/version")).await.unwrap();
        assert_eq!(response.status(), 200);
    }
}
