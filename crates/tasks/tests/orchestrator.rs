//! Orchestrator tick + HTTP integration tests. Real store, real child
//! processes standing in for headless Claude Code, real HTTP server. No mocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tasks::events::EventPayload;
use tasks::models::{ChatRole, GhState, Project, ProjectId, Task, TaskId, TaskState};
use tasks::orchestrator::{Orchestrator, OrchestratorConfig};
use tasks::run::orchestrator_nudge_loop;
use tasks::store::Store;
use tokio::sync::watch;

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

/// The watermark contract: input appended while the agent is mid-turn (its
/// seq below the eventual reply's) must stay unanswered, because the prompt
/// that turn was built from never included it.
#[tokio::test]
async fn input_arriving_mid_turn_stays_unanswered() {
    let store = Store::open_in_memory().await.unwrap();
    let first = store
        .append_orchestrator_message(ChatRole::User, "first")
        .await
        .unwrap();
    // The agent is "mid-turn" on `first` when more input lands:
    let late = store
        .append_orchestrator_message(ChatRole::Event, "[pipeline] spec landed")
        .await
        .unwrap();
    // The reply only covered `first`.
    store
        .append_orchestrator_reply("done", first.seq)
        .await
        .unwrap();

    let pending = store.unanswered_orchestrator_messages().await.unwrap();
    assert_eq!(
        pending.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![late.seq],
        "the mid-turn event turn is still pending; the answered one is not"
    );
}

/// End to end: pipeline events debounce into ONE `event` turn with
/// human-readable detail (issue number + title, not ids), noise events don't
/// nudge at all, and the next tick answers the nudge like any other input.
#[tokio::test]
async fn pipeline_events_become_one_event_turn_the_tick_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let nudge_loop = tokio::spawn(orchestrator_nudge_loop(
        store.clone(),
        Duration::from_millis(100),
        Duration::from_secs(2),
        shutdown_rx,
    ));
    // Let the loop subscribe before anything is published.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();
    let insert = async |number: u64, title: &str| {
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: title.into(),
            body: String::new(),
            labels: vec![],
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        };
        store.insert_task(&task).await.unwrap();
        task
    };
    let dark_mode = insert(12, "Add dark mode").await;
    let fix_login = insert(13, "Fix login redirect").await;

    for task in [&dark_mode, &fix_login] {
        store
            .append_event(EventPayload::TaskIngested {
                task_id: task.id.clone(),
                project_id: project.id.clone(),
            })
            .await
            .unwrap();
    }
    // No store lookups needed for this one — pure formatting.
    store
        .append_event(EventPayload::PullRequestOpened {
            build_id: tasks::models::BuildId::from_raw("build_x"),
            pr_number: 42,
        })
        .await
        .unwrap();
    // Noise: must not nudge (and must not appear in the turn).
    store
        .append_event(EventPayload::QueueReordered { task_ids: vec![] })
        .await
        .unwrap();

    // Wait for the debounced event turn.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let event_turns = loop {
        let turns: Vec<_> = store
            .orchestrator_messages_since(0)
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.role == ChatRole::Event)
            .collect();
        if !turns.is_empty() {
            // Debounce settled — give a beat to catch an (incorrect) second
            // turn before asserting there is exactly one.
            tokio::time::sleep(Duration::from_millis(300)).await;
            break store
                .orchestrator_messages_since(0)
                .await
                .unwrap()
                .into_iter()
                .filter(|m| m.role == ChatRole::Event)
                .collect::<Vec<_>>();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no event turn appeared within 5s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(event_turns.len(), 1, "burst debounced into one turn");
    let turn = &event_turns[0];
    assert!(turn.content.starts_with("[pipeline]"), "{}", turn.content);
    assert!(
        turn.content.contains("#12 \"Add dark mode\""),
        "{}",
        turn.content
    );
    assert!(
        turn.content.contains("#13 \"Fix login redirect\""),
        "{}",
        turn.content
    );
    assert!(turn.content.contains("PR #42 opened"), "{}", turn.content);
    assert!(!turn.content.contains("reorder"), "{}", turn.content);

    // The tick answers the nudge like any other input.
    assert!(orch.tick().await.unwrap());
    let messages = store.orchestrator_messages_since(0).await.unwrap();
    let last = messages.last().unwrap();
    assert_eq!(last.role, ChatRole::Assistant);
    assert!(
        last.content.contains("reply to: [pipeline]"),
        "{}",
        last.content
    );
    assert!(!orch.tick().await.unwrap(), "settled after the reply");

    nudge_loop.abort();
}

/// The interactive-checkout lifecycle over HTTP: no session → 409; with a
/// session, checkout marks it held (suspending ticks) and release clears it.
#[tokio::test]
async fn orchestrator_session_checkout_flows_over_http() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();

    let info: serde_json::Value = http
        .get(format!("{base}/orchestrator/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["cc_session_id"], serde_json::Value::Null);
    assert_eq!(info["checked_out"], false);

    // Nothing to check out yet.
    let resp = http
        .post(format!("{base}/orchestrator/session/checkout"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    store
        .set_orchestrator_cc_session(Some("cc-session-1"))
        .await
        .unwrap();
    store
        .set_orchestrator_workdir("/tmp/checkout")
        .await
        .unwrap();

    let info: serde_json::Value = http
        .post(format!("{base}/orchestrator/session/checkout"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["cc_session_id"], "cc-session-1");
    assert_eq!(info["workdir"], "/tmp/checkout");
    assert_eq!(info["checked_out"], true);
    assert!(store.orchestrator_checked_out().await.unwrap());

    let info: serde_json::Value = http
        .post(format!("{base}/orchestrator/session/release"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["checked_out"], false);
    assert!(!store.orchestrator_checked_out().await.unwrap());
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
