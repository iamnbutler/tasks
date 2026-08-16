//! A shutdown terminates, even while a client is tailing a stream.
//!
//! Real server on loopback, real SSE client, real `serve_on` — the graceful
//! shutdown path exactly as `tasks serve` uses it. Nothing here is mocked
//! because the bug was in the interaction: the drain finished in under a
//! millisecond and the process still needed a SIGKILL 75 seconds later, since
//! `axum::serve(..).with_graceful_shutdown(..)` waits for open connections and
//! `/events/stream` only ends when its client hangs up.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tasks::store::Store;

/// Bind a server, attach an SSE tail, then shut down and time it.
///
/// The assertion is a bound rather than an exact figure: what is under test is
/// that the wait is finite and short, and pinning `CONNECTION_GRACE` itself
/// here would make a deliberate change to it read as a regression.
#[tokio::test]
async fn a_client_tailing_a_stream_cannot_hold_the_shutdown_open() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(async move {
        tasks::server::serve_on(
            listener,
            store,
            tasks::server::Services::default(),
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    // A real tail, held open for the whole shutdown. `send()` resolves on the
    // response head, so by the time this returns the connection is established
    // and the body is still streaming — which is the state that used to hang.
    let response = reqwest::Client::new()
        .get(format!("{base}/events/stream"))
        .send()
        .await
        .expect("the event stream connects");
    assert!(response.status().is_success());

    let started = Instant::now();
    shutdown_tx.send(()).unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), served).await;

    // Held open, this is a 75s SIGKILL in production and a 30s timeout here.
    let outcome = outcome.expect("the server exits rather than waiting out the client");
    outcome.expect("the serve task did not panic").unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the shutdown waited {:?} on a client that never hangs up",
        started.elapsed(),
    );

    // The stream is severed, not politely finished: the client is a tailer and
    // reconnects to the successor with `?since=`. Dropping it last keeps the
    // connection open across the shutdown above rather than at the mercy of
    // when the runtime gets round to collecting it.
    drop(response);
}

/// The same shutdown with nothing attached still returns immediately — the
/// grace is a ceiling on waiting for clients, not a delay every stop pays.
#[tokio::test]
async fn an_idle_server_shuts_down_without_waiting_out_the_grace() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(async move {
        tasks::server::serve_on(
            listener,
            store,
            tasks::server::Services::default(),
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    // Let the accept loop reach its first await, so the shutdown is observed
    // by a running server rather than one that has not started serving.
    tokio::task::yield_now().await;

    let started = Instant::now();
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .expect("the idle server exits")
        .expect("the serve task did not panic")
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an idle shutdown paid {:?}; it should not wait for a grace it does \
         not need",
        started.elapsed(),
    );
}
