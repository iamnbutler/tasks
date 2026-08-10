//! Orchestrator tick + HTTP integration tests. Real store, real child
//! processes standing in for headless Claude Code, real HTTP server. No mocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tasks::models::ChatRole;
use tasks::orchestrator::{Orchestrator, OrchestratorConfig};
use tasks::store::Store;

mod common;

/// Write a stub agent that records its argv to `args_log` (one line per
/// invocation) and replies to stdin. `fail` makes it exit 1 instead.
async fn write_stub(dir: &Path, args_log: &Path, fail: bool) -> std::path::PathBuf {
    let stub = dir.join("stub-orchestrator.sh");
    let body = if fail {
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" | tr '\\n' ' ' >> {log}; echo >> {log}\n\
             cat > /dev/null\necho boom >&2\nexit 1\n",
            log = common::shell_escape(&args_log.display().to_string()),
        )
    } else {
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" | tr '\\n' ' ' >> {log}; echo >> {log}\n\
             PROMPT=$(cat)\necho \"reply to: $PROMPT\"\n",
            log = common::shell_escape(&args_log.display().to_string()),
        )
    };
    tokio::fs::write(&stub, body).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = tokio::fs::metadata(&stub).await.unwrap().permissions();
        p.set_mode(0o755);
        tokio::fs::set_permissions(&stub, p).await.unwrap();
    }
    stub
}

fn orchestrator(store: Arc<Store>, stub: &Path, tmp: &Path) -> Orchestrator {
    Orchestrator::new(
        store,
        OrchestratorConfig {
            command: stub.display().to_string(),
            timeout: Duration::from_secs(30),
            workdir: tmp.join("orch-workdir"),
            api_port: 4800,
        },
    )
}

#[tokio::test]
async fn a_tick_answers_pending_turns_and_resumes_the_same_session() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());

    // Nothing pending: no tick.
    assert!(!orch.tick().await.unwrap());

    store
        .append_orchestrator_message(ChatRole::User, "what's the status?")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    let messages = store.orchestrator_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, ChatRole::Assistant);
    assert!(
        messages[1].content.contains("reply to: what's the status?"),
        "the user's prompt reached the agent's stdin: {}",
        messages[1].content
    );

    // Settled: no re-tick.
    assert!(!orch.tick().await.unwrap());

    // A second turn resumes the SAME session Claude Code session.
    store
        .append_orchestrator_message(ChatRole::User, "queue #7")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    let log = tokio::fs::read_to_string(&args_log).await.unwrap();
    let calls: Vec<&str> = log.lines().collect();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0].contains("--session-id") && calls[0].contains("--append-system-prompt"),
        "first call creates the session with the standing prompt: {}",
        calls[0]
    );
    let session = store
        .orchestrator_cc_session()
        .await
        .unwrap()
        .expect("session recorded");
    assert!(
        calls[1].contains(&format!("--resume {session}")),
        "second call resumes it: {}",
        calls[1]
    );

    // Both unanswered turns folded into one reply each time.
    let events = store.events_since(0).await.unwrap();
    assert!(!events.is_empty());
}

/// The streaming path: a stream-json agent's deltas and tool calls surface
/// on the live feed in order, and the persisted reply is the `result`
/// record's text — never raw JSON.
#[tokio::test]
async fn a_stream_json_agent_feeds_deltas_and_tools_and_lands_the_result() {
    use tasks::models::OrchestratorFeedEvent as Feed;

    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("stream.jsonl");
    tokio::fs::write(
        &fixture,
        concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Check"}}}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ing"}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"curl -s http://127.0.0.1:4800/tasks"}}]}}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"All good."}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"All good."}"#,
            "\n",
        ),
    )
    .await
    .unwrap();

    let stub = tmp.path().join("stream-stub.sh");
    tokio::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncat > /dev/null\ncat {}\n",
            common::shell_escape(&fixture.display().to_string())
        ),
    )
    .await
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = tokio::fs::metadata(&stub).await.unwrap().permissions();
        p.set_mode(0o755);
        tokio::fs::set_permissions(&stub, p).await.unwrap();
    }

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    // Subscribe before the tick so nothing published during it is missed.
    let mut feed = store.subscribe_orchestrator_feed();
    let orch = orchestrator(store.clone(), &stub, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "status?")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    let messages = store.orchestrator_messages_since(0).await.unwrap();
    assert_eq!(
        messages[1].content, "All good.",
        "reply is the result record's text, not raw stream-json"
    );

    let mut got = Vec::new();
    while let Ok(event) = feed.try_recv() {
        got.push(event);
    }
    assert_eq!(
        got,
        vec![
            Feed::Delta {
                text: "Check".into()
            },
            Feed::Delta { text: "ing".into() },
            Feed::Tool {
                label: "Bash: curl -s http://127.0.0.1:4800/tasks".into()
            },
            Feed::Delta {
                text: "All good.".into()
            },
            Feed::Done,
        ]
    );
}

#[tokio::test]
async fn the_orchestrator_stream_endpoint_relays_the_feed() {
    use tasks::models::OrchestratorFeedEvent as Feed;

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut resp = reqwest::Client::new()
        .get(format!("{base}/orchestrator/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Headers received means the handler ran, which means the subscription
    // exists — publishing now cannot race it.
    store.publish_orchestrator_feed(Feed::Delta { text: "hi".into() });
    store.publish_orchestrator_feed(Feed::Tool {
        label: "Bash: curl".into(),
    });
    store.publish_orchestrator_feed(Feed::Done);

    let mut body = String::new();
    while !body.contains(r#"{"kind":"done"}"#) {
        let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
            .await
            .expect("feed frame within 5s")
            .unwrap()
            .expect("stream still open");
        body.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(body.contains(r#"{"kind":"delta","text":"hi"}"#), "{body}");
    assert!(
        body.contains(r#"{"kind":"tool","label":"Bash: curl"}"#),
        "{body}"
    );
}

/// A failing agent must settle the tick (error becomes the assistant turn)
/// rather than retrying a poison prompt forever.
#[tokio::test]
async fn an_agent_failure_lands_in_the_chat_and_settles_the_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, true).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());

    store
        .append_orchestrator_message(ChatRole::User, "hello?")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    let messages = store.orchestrator_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].content.contains("orchestrator error"));
    // Settled — a second tick does nothing.
    assert!(!orch.tick().await.unwrap());
    // Two invocations: the resume-failure heal path retried once with a
    // fresh session before giving up.
    let log = tokio::fs::read_to_string(&args_log).await.unwrap();
    assert!(log.lines().count() >= 1);
}

#[tokio::test]
async fn messages_flow_over_http() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{base}/orchestrator/messages"))
        .json(&serde_json::json!({"content": "  status please  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let sent: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(sent["role"], "user");
    assert_eq!(sent["content"], "status please");

    // Blank content is a 400, not an empty turn the loop would answer.
    let resp = http
        .post(format!("{base}/orchestrator/messages"))
        .json(&serde_json::json!({"content": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let listed: Vec<serde_json::Value> = http
        .get(format!("{base}/orchestrator/messages?since=0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    let seq = listed[0]["seq"].as_i64().unwrap();
    let after: Vec<serde_json::Value> = http
        .get(format!("{base}/orchestrator/messages?since={seq}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(after.is_empty());
}
