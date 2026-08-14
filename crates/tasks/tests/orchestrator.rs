//! Orchestrator tick + HTTP integration tests. Real store, real child
//! processes standing in for headless Claude Code, real HTTP server. No mocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tasks::events::EventPayload;
use tasks::models::{
    ChatRole, Complexity, GhState, Project, ProjectId, Session, SessionEndReason, SessionId,
    SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
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

    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
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
    let events = store.all_events().await.unwrap();
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

    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
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

    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].content.contains("orchestrator error"));
    // Settled — a second tick does nothing.
    assert!(!orch.tick().await.unwrap());
    // Two invocations: the resume-failure heal path retried once with a
    // fresh session before giving up.
    let log = tokio::fs::read_to_string(&args_log).await.unwrap();
    assert!(log.lines().count() >= 1);
}

/// Losing the agent's context must stop being invisible. A failed `--resume`
/// ends the session in the ledger, writes a seam the reader can see, and
/// starts a fresh session — and the seam must NOT become input, or the new
/// session spends its first turn acknowledging its own amnesia.
#[tokio::test]
async fn a_lost_session_leaves_a_visible_seam_and_a_ledger_row() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let store = Arc::new(Store::open_in_memory().await.unwrap());

    // First tick: no session yet, so this one succeeds and is adopted.
    let good = write_stub(tmp.path(), &args_log, false).await;
    let orch = orchestrator(store.clone(), &good, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "hello")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());
    let first_session = store.orchestrator_cc_session().await.unwrap().unwrap();

    // Second tick: an agent that refuses to resume but is happy to start
    // fresh — exactly the shape of a lost Claude Code session.
    let picky = tmp.path().join("picky.sh");
    tokio::fs::write(
        &picky,
        "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"--resume\" ]; then\n    \
         cat > /dev/null; echo 'no such session' >&2; exit 1\n  fi\ndone\n\
         cat > /dev/null\necho 'starting over'\n",
    )
    .await
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = tokio::fs::metadata(&picky).await.unwrap().permissions();
        p.set_mode(0o755);
        tokio::fs::set_permissions(&picky, p).await.unwrap();
    }
    let orch = orchestrator(store.clone(), &picky, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "still there?")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    // The chat carries the seam, between the question and the fresh reply.
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    let seam = messages
        .iter()
        .find(|m| m.role == ChatRole::System)
        .expect("a seam turn was written");
    assert!(
        seam.content.contains("context"),
        "the seam says what was lost: {}",
        seam.content
    );
    assert_eq!(
        messages.last().unwrap().content,
        "starting over",
        "the fresh session's reply lands after the seam"
    );

    // ...but it is not input: the conversation is settled, so no tick fires
    // to answer the notice of the restart.
    assert!(
        store
            .unanswered_orchestrator_messages()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!orch.tick().await.unwrap());

    // The ledger records both regimes, and which one died how.
    let second_session = store.orchestrator_cc_session().await.unwrap().unwrap();
    assert_ne!(second_session, first_session, "a new session was adopted");
    let sessions = store.orchestrator_sessions().await.unwrap();
    let old = sessions
        .iter()
        .find(|s| s.cc_session_id == first_session)
        .unwrap();
    assert_eq!(old.end_reason, Some(SessionEndReason::ResumeFailed));
    assert!(old.ended_at.is_some());
    let new = sessions
        .iter()
        .find(|s| s.cc_session_id == second_session)
        .unwrap();
    assert!(new.ended_at.is_none(), "the new session is live");

    // And the seam is on the wire for anything watching.
    let events = store.events_since(0, 200).await.unwrap();
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::OrchestratorSessionStarted {
                session_id,
                replacing,
                ..
            } => Some((session_id.clone(), replacing.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![
            (first_session.clone(), None),
            (second_session.clone(), Some(first_session)),
        ]
    );
}

/// The context gauge: an agent reporting stream-json usage advances it, and
/// the reading is the whole input side — cached tokens included, since on a
/// long resumed session the cache is nearly all of it.
#[tokio::test]
async fn a_usage_reporting_agent_advances_the_context_gauge() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("stream.jsonl");
    tokio::fs::write(
        &fixture,
        concat!(
            r#"{"type":"result","subtype":"success","result":"ok","usage":"#,
            r#"{"input_tokens":1200,"cache_read_input_tokens":180000,"#,
            r#""cache_creation_input_tokens":800,"output_tokens":450}}"#,
            "\n",
        ),
    )
    .await
    .unwrap();
    let stub = tmp.path().join("usage-stub.sh");
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
    let orch = orchestrator(store.clone(), &stub, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "status?")
        .await
        .unwrap();
    assert!(orch.tick().await.unwrap());

    let info = store.orchestrator_session_info().await.unwrap();
    assert_eq!(
        info.context_tokens,
        Some(182_000),
        "input + cache_read + cache_creation, not input_tokens alone"
    );
    // The reply itself is attributed to the session that produced it.
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.last().unwrap().content, "ok");
}

/// Attribution over the wire: a caller presenting the minted token is the
/// orchestrator and owes a rationale; everyone else is the human and owes
/// nothing. Getting this wrong would misroute the self-nudge filter, so it is
/// checked end to end rather than at the store.
#[tokio::test]
async fn the_actor_header_decides_who_a_write_belongs_to() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let spec = seed_pending_spec(&store).await;
    let token = store.actor_token().to_string();

    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let review_url = format!("{base}/spec-queue/{}/review", spec.id);

    // Presenting the token without a reason is refused.
    let resp = http
        .post(&review_url)
        .header("X-Tasks-Actor", format!("orchestrator {token}"))
        .json(&serde_json::json!({"status": "approved"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "an autonomous verdict needs a rationale"
    );

    // A wrong token is simply the human, who owes no reason.
    let resp = http
        .post(&review_url)
        .header("X-Tasks-Actor", "orchestrator not-the-token")
        .json(&serde_json::json!({"status": "approved"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let decisions: Vec<serde_json::Value> = http
        .get(format!("{base}/decisions?spec={}", spec.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "the refused attempt left no trace");
    assert_eq!(decisions[0]["actor"], "human");
    assert_eq!(decisions[0]["action"], "approve");

    // And with the real token plus a reason, the ledger says orchestrator.
    let resp = http
        .post(format!("{base}/builds"))
        .header("X-Tasks-Actor", format!("orchestrator {token}"))
        .json(&serde_json::json!({
            "spec_ids": [spec.id],
            "rationale": "only approved spec; base is clean",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let decisions: Vec<serde_json::Value> = http
        .get(format!("{base}/decisions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(decisions[0]["actor"], "orchestrator");
    assert_eq!(decisions[0]["action"], "request_build");
    assert_eq!(
        decisions[0]["rationale"],
        "only approved spec; base is clean"
    );
}

/// End to end: an obligation reaches the conversation as an *input* turn the
/// tick answers (unlike a seam, which is written for the reader only), stops
/// repeating while it is fresh, and stays open until a decision — not a
/// mention — discharges it.
#[tokio::test]
async fn standing_obligations_reach_the_conversation_and_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let spec = seed_pending_spec(&store).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_handle = tokio::spawn(tasks::run::obligation_loop(
        store.clone(),
        Duration::from_millis(0),  // nothing is "fresh" here
        Duration::from_secs(3600), // ...and one mention is enough
        Duration::from_millis(20),
        shutdown_rx,
    ));

    // The obligation lands as a turn.
    let turn = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
            if let Some(m) = messages.iter().find(|m| m.role == ChatRole::Event) {
                return m.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an obligation turn was appended");
    assert!(
        turn.content.contains("Standing obligations"),
        "distinguishable from a notification: {}",
        turn.content
    );
    assert!(turn.content.contains("waiting for a verdict"));

    // It is input — the tick owes it a reply, unlike a session seam.
    let pending = store.unanswered_orchestrator_messages().await.unwrap();
    assert!(pending.iter().any(|m| m.seq == turn.seq));
    let orch = orchestrator(store.clone(), &stub, tmp.path());
    assert!(orch.tick().await.unwrap());

    // Answering is not deciding: the obligation is still open.
    assert!(
        !store
            .open_obligations(chrono::Duration::zero())
            .await
            .unwrap()
            .is_empty(),
        "a reply does not discharge an obligation — only a decision does"
    );

    // And it is not repeated while the reminder interval holds.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let event_turns = store
        .orchestrator_messages_since(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role == ChatRole::Event)
        .count();
    assert_eq!(event_turns, 1, "one mention, not one per tick");

    // A verdict is what ends it.
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    assert!(
        store
            .open_obligations(chrono::Duration::zero())
            .await
            .unwrap()
            .is_empty()
    );

    let _ = shutdown_tx.send(true);
    let _ = loop_handle.await;
}

/// A spec sitting in `pending_review`, with the task and session behind it.
async fn seed_pending_spec(store: &Store) -> Spec {
    let now = Utc::now();
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: now,
    };
    store.insert_project(&project).await.unwrap();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 1,
        title: "a task".into(),
        body: "body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::InReview,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: None,
        branch: "scout/1".into(),
        status: SessionStatus::ScoutSucceeded,
        started_at: now,
        completed_at: Some(now),
        exit_reason: None,
        usage: None,
    };
    store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: session.id,
        task_id: task.id,
        content: "## Spec\n\nDo the thing.".into(),
        complexity: Complexity::Simple,
        files_touched: vec![],
        created_at: now,
    };
    store.insert_spec(&spec).await.unwrap();
    store
        .upsert_spec_queue_entry(&SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::PendingReview,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        })
        .await
        .unwrap();
    spec
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
        .append_orchestrator_reply("done", first.seq, None)
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
            .orchestrator_messages_since(0, 1000)
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
                .orchestrator_messages_since(0, 1000)
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
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
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
        .begin_orchestrator_session("cc-session-1", None, None)
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
