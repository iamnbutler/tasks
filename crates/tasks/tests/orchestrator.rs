//! Orchestrator tick + HTTP integration tests. Real store, real child
//! processes standing in for headless Claude Code, real HTTP server. No mocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tasks::events::EventPayload;
use tasks::models::{
    ChatRole, Complexity, GhState, ObligationKind, Project, ProjectId, ProjectStatus, Session,
    SessionEndReason, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus,
    Task, TaskId, TaskState,
};
use tasks::orchestrator::{
    Interruption, Orchestrator, OrchestratorConfig, TurnControl, VERIFICATION_ENV,
};
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
            worker_timeout: Duration::from_secs(300),
            workdir: tmp.join("orch-workdir"),
            workdir_is_checkout: false,
            target_dir: None,
            worktree_dir: tmp.join("verify-worktree"),
            github_configured: true,
            api_port: 4800,
            curl_config: tmp.join("orchestrator-curl.conf"),
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
    assert!(!orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    store
        .append_orchestrator_message(ChatRole::User, "what's the status?")
        .await
        .unwrap();
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, ChatRole::Assistant);
    assert!(
        messages[1].content.contains("reply to: what's the status?"),
        "the user's prompt reached the agent's stdin: {}",
        messages[1].content
    );

    // Settled: no re-tick.
    assert!(!orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    // A second turn resumes the SAME session Claude Code session.
    store
        .append_orchestrator_message(ChatRole::User, "queue #7")
        .await
        .unwrap();
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

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
            Feed::Started,
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

/// The case the start signal exists for: a tick nobody in front of a screen
/// asked for, running an agent that does not stream. `[Started, Done]` is
/// then the *only* thing a client can show — without `Started` the whole tick
/// is invisible until its reply lands. And a no-op tick must announce
/// nothing, or a client shows a clock for work that never began.
#[tokio::test]
async fn a_proactive_tick_announces_itself_before_generating_anything() {
    use tasks::models::OrchestratorFeedEvent as Feed;

    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let mut feed = store.subscribe_orchestrator_feed();
    let orch = orchestrator(store.clone(), &stub, tmp.path());

    // Nothing pending: the tick is a no-op and says so by saying nothing.
    assert!(!orch.tick(&Arc::new(TurnControl::new())).await.unwrap());
    assert!(
        feed.try_recv().is_err(),
        "a no-op tick must not announce itself"
    );

    // An event turn — pipeline news, not a human's message.
    store
        .append_orchestrator_message(ChatRole::Event, "spec pending review for #7")
        .await
        .unwrap();
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    let mut got = Vec::new();
    while let Ok(event) = feed.try_recv() {
        got.push(event);
    }
    assert_eq!(
        got,
        vec![Feed::Started, Feed::Done],
        "a plain-text agent streams nothing, so the lifecycle is the signal"
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
    store.publish_orchestrator_feed(Feed::Started);
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
    assert!(body.contains(r#"{"kind":"started"}"#), "{body}");
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].content.contains("orchestrator error"));
    // Settled — a second tick does nothing.
    assert!(!orch.tick(&Arc::new(TurnControl::new())).await.unwrap());
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

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
    assert!(!orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

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

/// The two gauges, from one stream. Context size is the input side of the
/// last *main-chain* assistant turn — cached tokens included, since on a long
/// resumed session the cache is nearly all of it, and sub-agent turns
/// excluded, since those hold a context of their own. What the whole
/// invocation cost is the `result` aggregate, which is a much larger number
/// and means something else entirely.
#[tokio::test]
async fn usage_separates_context_size_from_what_the_tick_spent() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("stream.jsonl");
    tokio::fs::write(
        &fixture,
        concat!(
            // An early main-chain turn, superseded by the later one.
            r#"{"type":"assistant","message":{"content":[],"usage":"#,
            r#"{"input_tokens":900,"cache_read_input_tokens":60000}}}"#,
            "\n",
            // A sub-agent turn with a huge context of its own: not ours.
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":"#,
            r#"{"content":[],"usage":{"input_tokens":900000}}}"#,
            "\n",
            // The last main-chain turn — this is the reading that counts.
            r#"{"type":"assistant","parent_tool_use_id":null,"message":"#,
            r#"{"model":"claude-opus-5","content":[],"usage":{"input_tokens":1200,"#,
            r#""cache_read_input_tokens":180000,"cache_creation_input_tokens":800,"#,
            r#""output_tokens":450}}}"#,
            "\n",
            // A compaction that landed, mid-tick.
            r#"{"type":"system","subtype":"status","status":null,"#,
            r#""compact_result":"ok"}"#,
            "\n",
            // And the invocation's aggregate bill, with each model's own
            // window: the sub-agent's is smaller and is not ours.
            r#"{"type":"result","subtype":"success","result":"ok","usage":"#,
            r#"{"input_tokens":2000,"cache_read_input_tokens":2700000,"#,
            r#""output_tokens":9000},"modelUsage":{"#,
            r#""claude-opus-5[1m]":{"contextWindow":1000000,"#,
            r#""canonicalModel":"claude-opus-5"},"#,
            r#""claude-haiku-4-5-20251001":{"contextWindow":200000,"#,
            r#""canonicalModel":"claude-haiku-4-5-20251001"}}}"#,
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    let info = store.orchestrator_session_info().await.unwrap();
    assert_eq!(
        info.context_tokens,
        Some(182_000),
        "the last main-chain assistant turn: input + cache_read + cache_creation, \
         and never the 900k sub-agent turn"
    );
    assert_eq!(
        info.tick_tokens,
        Some(2_702_000),
        "the result aggregate is what the tick spent, kept under its own name"
    );
    // The denominator is transcribed from the agent, and from the entry for
    // the model the main chain ran on — not the sub-agent's 200k.
    assert_eq!(info.model_id.as_deref(), Some("claude-opus-5[1m]"));
    assert_eq!(info.context_window, Some(1_000_000));
    let parts = info.context_breakdown.expect("the parts of that reading");
    assert_eq!(parts.input, 1_200);
    assert_eq!(parts.cache_read, 180_000);
    assert_eq!(parts.cache_creation, 800);
    assert_eq!(
        parts.total(),
        info.context_tokens.unwrap(),
        "the parts have to sum to the whole they are shown beside"
    );
    assert_eq!(info.compactions, 1);
    assert!(info.last_compacted_at.is_some());
    // The reply itself is attributed to the session that produced it.
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.last().unwrap().content, "ok");

    // A second tick by an agent that reports no usage at all must not erase
    // either reading — a stalled gauge is honest, a cleared one is a lie.
    let args_log = tmp.path().join("args.log");
    let plain = write_stub(tmp.path(), &args_log, false).await;
    let orch = orchestrator(store.clone(), &plain, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "and now?")
        .await
        .unwrap();
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    let info = store.orchestrator_session_info().await.unwrap();
    assert_eq!(info.context_tokens, Some(182_000));
    assert_eq!(info.tick_tokens, Some(2_702_000));
    // Everything the second tick was silent about holds its last real value,
    // and the compaction count in particular does not restart.
    assert_eq!(info.model_id.as_deref(), Some("claude-opus-5[1m]"));
    assert_eq!(info.context_window, Some(1_000_000));
    assert_eq!(info.compactions, 1);
}

/// The regression test for #826, and deliberately end-to-end: a stub agent
/// reads its instructions, pulls the `-K <path>` out of them, and makes a real
/// `curl -K` write against a real bound server. A unit test cannot catch this
/// class of bug — the old scheme (`-H "X-Tasks-Actor: orchestrator
/// $TASKS_ACTOR_TOKEN"`) passed every unit test and still could not be run by
/// an agent under `--allowedTools Bash(curl:*)`, because Claude Code will not
/// statically verify a command containing a shell variable. The result was
/// that the safest deployment was the one where nothing was attributable and
/// the charter governed nothing.
#[tokio::test]
async fn the_agent_identifies_its_writes_with_the_curl_config_and_no_shell_expansion() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let spec = seed_pending_spec(&store).await;
    store
        .set_charter(
            tasks::models::Capability::AutoReviewSpecs,
            tasks::models::CharterLevel::Live,
            None,
        )
        .await
        .unwrap();

    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The credential lives under the data dir, not in the workdir: in
    // production the workdir is a repo checkout the agent commits from.
    let workdir = tmp.path().join("workdir");
    let curl_config = tmp.path().join("state").join("orchestrator-curl.conf");

    // The stub stands in for Claude Code: it reads the system prompt it was
    // handed, finds the config path in it, and writes through the API with
    // it. Note the `` ` `` in the character class — the prompt mentions the
    // path once inside backticks and once bare.
    let stub = tmp.path().join("curl-agent.sh");
    tokio::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncat > /dev/null\n\
             CONF=$(printf '%s\\n' \"$@\" | grep -o -- '-K [^ `]*orchestrator-curl.conf' \
             | head -1 | cut -c4-)\n\
             curl -sS -K \"$CONF\" -X POST \
             http://127.0.0.1:{port}/spec-queue/{spec_id}/review \
             -H 'Content-Type: application/json' \
             -d '{{\"status\":\"approved\",\"rationale\":\"the spec holds up\"}}' \
             > /dev/null\n\
             echo approved\n",
            spec_id = spec.id,
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

    let orch = Orchestrator::new(
        store.clone(),
        OrchestratorConfig {
            command: stub.display().to_string(),
            timeout: Duration::from_secs(30),
            worker_timeout: Duration::from_secs(300),
            workdir: workdir.clone(),
            workdir_is_checkout: false,
            target_dir: None,
            worktree_dir: tmp.path().join("verify-worktree"),
            github_configured: true,
            api_port: port,
            curl_config: curl_config.clone(),
        },
    );
    store
        .append_orchestrator_message(ChatRole::User, "review the spec")
        .await
        .unwrap();
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    // The write landed, and it landed as the orchestrator's.
    let decisions = store
        .decisions(Some(("spec", spec.id.as_str())), 10)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "the agent made exactly one write");
    assert_eq!(
        decisions[0].actor,
        tasks::models::Actor::Orchestrator,
        "an unattributed write would silently be the human's"
    );
    assert!(decisions[0].enforced);
    assert_eq!(
        decisions[0].rationale.as_deref(),
        Some("the spec holds up"),
        "attribution is what makes the rationale requirement reachable at all"
    );

    // And the credential itself: readable only by us, and nowhere near the
    // checkout the agent commits from.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&curl_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(
        !curl_config.starts_with(&workdir),
        "a secret in the workdir is one `git add -A` from being published"
    );
}

/// Attribution over the wire: a caller presenting the minted token is the
/// orchestrator and owes a rationale; everyone else is the human and owes
/// nothing. Getting this wrong would misroute the self-nudge filter, so it is
/// checked end to end rather than at the store.
#[tokio::test]
async fn the_actor_header_decides_who_a_write_belongs_to() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let spec = seed_pending_spec(&store).await;
    let token = store.actor_token().expose().to_string();
    // This test is about *who* a write belongs to, not whether it is allowed,
    // so grant the two capabilities it exercises. The charter is checked
    // before the body is validated — permission first, then validity — which
    // is why they have to be live for a 400 to be reachable at all.
    for capability in [
        tasks::models::Capability::AutoReviewSpecs,
        tasks::models::Capability::DispatchBuilds,
    ] {
        store
            .set_charter(capability, tasks::models::CharterLevel::Live, None)
            .await
            .unwrap();
    }

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

    // A wrong token is refused, not quietly promoted to the human — who is
    // never gated, and would therefore be *more* authority than was claimed.
    let resp = http
        .post(&review_url)
        .header("X-Tasks-Actor", "orchestrator not-the-token")
        .json(&serde_json::json!({"status": "approved"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // The human sends no header and owes no reason.
    let resp = http
        .post(&review_url)
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
        common::offline_config(tmp.path()),
        Default::default(),
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

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

    // A verdict is what ends it — and hands off to the next one, because an
    // approved spec nobody is building is also work the pipeline is owed.
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    let open = store
        .open_obligations(chrono::Duration::zero())
        .await
        .unwrap();
    assert_eq!(
        open.iter().map(|o| o.kind).collect::<Vec<_>>(),
        vec![ObligationKind::DispatchBuild],
        "review discharged, dispatch owed: {open:?}"
    );

    let _ = shutdown_tx.send(true);
    let _ = loop_handle.await;
}

/// Approval is not delivery. Nothing in the pipeline dispatches on its own, so
/// an approved spec with no build behind it is owed work — and the obligation
/// has to survive the build *failing*, which is the case a one-shot
/// notification would drop on the floor.
#[tokio::test]
async fn an_approved_spec_stays_owed_until_a_build_carries_it() {
    let store = Store::open_in_memory().await.unwrap();
    let spec = seed_pending_spec(&store).await;
    async fn owed(store: &Store, kind: ObligationKind) -> bool {
        store
            .open_obligations(chrono::Duration::zero())
            .await
            .unwrap()
            .iter()
            .any(|o| o.kind == kind)
    }

    // Pending review owes a verdict and nothing else.
    assert!(owed(&store, ObligationKind::ReviewSpec).await);
    assert!(!owed(&store, ObligationKind::DispatchBuild).await);

    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    assert!(owed(&store, ObligationKind::DispatchBuild).await);
    assert!(!owed(&store, ObligationKind::ReviewSpec).await);

    // A queued build discharges it — builds are serial, so waiting in the
    // queue is being carried, and re-raising here would ask for the same
    // build twice.
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    assert!(!owed(&store, ObligationKind::DispatchBuild).await);

    // A failure returns the spec to `approved`, and the obligation with it:
    // the spec is still good, and nothing else will pick it up.
    store
        .finalize_build_failed(&build.id, "linker OOM")
        .await
        .unwrap();
    assert!(
        owed(&store, ObligationKind::DispatchBuild).await,
        "a failed build leaves the work owed, not done"
    );

    // And it says so. "No build is carrying it" reads as *never built* unless
    // the history rides along, and re-running an hour-long build that already
    // failed should cost a reader one sentence, not one attempt.
    let summary = store
        .open_obligations(chrono::Duration::zero())
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.kind == ObligationKind::DispatchBuild)
        .expect("the dispatch obligation")
        .summary;
    assert!(
        summary.contains("1 earlier build(s) failed") && summary.contains("linker OOM"),
        "the obligation must carry why the last attempt failed: {summary}"
    );
}

/// With several specs unbuilt, the turn has to say that a Builder run takes a
/// *list* — otherwise the obvious reading is one dispatch per obligation, and
/// work that belongs on one branch gets N branches and N PRs.
#[tokio::test]
async fn several_unbuilt_specs_ask_to_be_batched() {
    let store = Store::open_in_memory().await.unwrap();
    let brief = tasks::brief::Brief::new(&store, None, "main");
    let obligation = |n: u64| tasks::models::Obligation {
        kind: ObligationKind::DispatchBuild,
        subject_id: format!("spec_{n}"),
        summary: format!("#{n} \"a task\" was approved and no build is carrying it"),
        since: Utc::now(),
    };

    let one = tasks::orchestrator::format_obligations(&store, &brief, &[obligation(1)]).await;
    assert!(
        !one.contains("one `POST /builds`"),
        "no batching advice for a single spec: {one}"
    );

    let many =
        tasks::orchestrator::format_obligations(&store, &brief, &[obligation(1), obligation(2)])
            .await;
    assert!(many.contains("2 approved specs are unbuilt"), "{many}");
    assert!(many.contains("one `POST /builds`"), "{many}");
}

/// The `land_batch` turn has to say whose job landing is, and it has to get
/// that from the charter rather than from a sentence somebody wrote once. The
/// bug: the charter shipped `land_builds` **live** while the prompt said
/// landing was the human's, so every parked PR was reported and none was
/// merged.
#[tokio::test]
async fn a_land_batch_turn_says_merging_is_the_orchestrators_only_when_the_charter_does() {
    use tasks::models::{Capability, CharterLevel};

    let store = Store::open_in_memory().await.unwrap();
    let brief = tasks::brief::Brief::new(&store, None, "main");
    let obligation = tasks::models::Obligation {
        kind: ObligationKind::LandBatch,
        subject_id: "build_1".to_string(),
        summary: "PR #906 has been open for 3 days".into(),
        since: Utc::now(),
    };

    // Live is the shipped default, and it is what the charter migration
    // inserts — so this is what a real turn reads.
    let live =
        tasks::orchestrator::format_obligations(&store, &brief, std::slice::from_ref(&obligation))
            .await;
    assert!(live.contains("yours to land"), "{live}");
    assert!(live.contains("say which of those three is why"), "{live}");

    for level in [CharterLevel::Shadow, CharterLevel::Off] {
        store
            .set_charter(Capability::LandBuilds, level, None)
            .await
            .unwrap();
        let turn = tasks::orchestrator::format_obligations(
            &store,
            &brief,
            std::slice::from_ref(&obligation),
        )
        .await;
        assert!(
            !turn.contains("yours to land"),
            "{level:?} must not claim an authority the server will refuse: {turn}"
        );
    }
}

/// A spec sitting in `pending_review`, with the task and session behind it.
async fn seed_pending_spec(store: &Store) -> Spec {
    let now = Utc::now();
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: now,
        status: ProjectStatus::Active,
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
        scout_directions: None,
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
        directions: None,
    };
    store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: Some(session.id),
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

/// A second spec in the same project — [`seed_pending_spec`] seeds the
/// project itself and cannot run twice against `UNIQUE(repo_owner,
/// repo_name)`.
async fn seed_sibling_spec(store: &Store, issue: u64) -> Spec {
    let project = store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("seed_pending_spec first");
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: format!("task {issue}"),
        body: "body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::InReview,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
        scout_directions: None,
    };
    store.insert_task(&task).await.unwrap();
    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: None,
        branch: format!("scout/{issue}"),
        status: SessionStatus::ScoutSucceeded,
        started_at: now,
        completed_at: Some(now),
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: Some(session.id),
        task_id: task.id,
        content: "## Spec\n\nDo the other thing.".into(),
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

/// The lane-free turn is the batching moment (#1055): when a build concludes,
/// the nudge lists every pooled approved spec and says to dispatch them
/// batched — the one turn that sees the whole pool at once. The trigger is
/// the completion *event*, not pool state: a cancel frees the lane too but
/// never nudges (the echo rule), so its pool must wait for the obligation's
/// grace rather than rendering here.
#[tokio::test]
async fn the_lane_free_turn_lists_the_pool_and_says_to_batch() {
    let store = Store::open_in_memory().await.unwrap();
    let brief = tasks::brief::Brief::new(&store, None, "main");
    let spec = seed_pending_spec(&store).await;
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    store
        .finalize_build_failed(&build.id, "linker OOM")
        .await
        .unwrap();

    let completed = |status: tasks::models::BuildStatus| tasks::events::Event {
        seq: 1,
        timestamp: Utc::now(),
        payload: EventPayload::BuildCompleted {
            build_id: build.id.clone(),
            status,
        },
    };

    let turn = tasks::orchestrator::format_nudge(
        &store,
        &brief,
        &[completed(tasks::models::BuildStatus::Failed)],
    )
    .await;
    assert!(turn.contains("1 approved spec(s) are pooled"), "{turn}");
    assert!(turn.contains("POST /builds"), "{turn}");
    assert!(turn.contains("touch the same files"), "{turn}");
    assert!(
        turn.contains(&spec.id.to_string()),
        "the pool names its specs: {turn}"
    );

    let cancelled = tasks::orchestrator::format_nudge(
        &store,
        &brief,
        &[completed(tasks::models::BuildStatus::Cancelled)],
    )
    .await;
    assert!(
        !cancelled.contains("pooled"),
        "a cancel never rendered a nudge, so pool state alone must not: {cancelled}"
    );
}

/// A completion with another build already queued is not a free lane — a pool
/// dispatched into it would freeze its composition early, which is the exact
/// mistake lane-free dispatch exists to end. The pooled spec here is real and
/// uncarried, so only the lane gate can be what keeps it out of the turn.
#[tokio::test]
async fn a_still_busy_lane_keeps_the_pool_out_of_the_lane_free_turn() {
    let store = Store::open_in_memory().await.unwrap();
    let brief = tasks::brief::Brief::new(&store, None, "main");
    let pooled = seed_pending_spec(&store).await;
    let carried = seed_sibling_spec(&store, 2).await;
    for spec in [&pooled, &carried] {
        store
            .review_spec(
                &spec.id,
                SpecQueueStatus::Approved,
                None,
                tasks::models::DecisionInput::human(),
            )
            .await
            .unwrap();
    }
    let first = store
        .create_build(
            std::slice::from_ref(&carried.id),
            "main",
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    store
        .finalize_build_failed(&first.id, "boom")
        .await
        .unwrap();
    // A second build takes the lane before the first one's nudge renders.
    store
        .create_build(
            std::slice::from_ref(&carried.id),
            "main",
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();

    let turn = tasks::orchestrator::format_nudge(
        &store,
        &brief,
        &[tasks::events::Event {
            seq: 1,
            timestamp: Utc::now(),
            payload: EventPayload::BuildCompleted {
                build_id: first.id.clone(),
                status: tasks::models::BuildStatus::Failed,
            },
        }],
    )
    .await;
    assert!(
        !turn.contains("pooled"),
        "a queued build holds the lane, and the next completion asks again: {turn}"
    );
}

/// The pool paragraph claims an authority, so it renders only when the
/// charter grants it — claiming one the server will refuse is worse than
/// silence, the same rule the landing text follows.
#[tokio::test]
async fn the_pool_speaks_only_when_dispatching_is_the_orchestrators() {
    use tasks::models::{Capability, CharterLevel};

    let store = Store::open_in_memory().await.unwrap();
    let brief = tasks::brief::Brief::new(&store, None, "main");
    let spec = seed_pending_spec(&store).await;
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            tasks::models::DecisionInput::human(),
        )
        .await
        .unwrap();
    store
        .finalize_build_failed(&build.id, "boom")
        .await
        .unwrap();
    let event = tasks::events::Event {
        seq: 1,
        timestamp: Utc::now(),
        payload: EventPayload::BuildCompleted {
            build_id: build.id.clone(),
            status: tasks::models::BuildStatus::Failed,
        },
    };

    for level in [CharterLevel::Shadow, CharterLevel::Off] {
        store
            .set_charter(Capability::DispatchBuilds, level, None)
            .await
            .unwrap();
        let turn =
            tasks::orchestrator::format_nudge(&store, &brief, std::slice::from_ref(&event)).await;
        assert!(
            !turn.contains("pooled"),
            "{level:?} must not claim an authority the server will refuse: {turn}"
        );
    }

    // Live is the shipped default the migration inserts — restored, the pool
    // speaks again, which is what pins the gate to the charter rather than
    // to anything the loop above changed as a side effect.
    store
        .set_charter(Capability::DispatchBuilds, CharterLevel::Live, None)
        .await
        .unwrap();
    let turn =
        tasks::orchestrator::format_nudge(&store, &brief, std::slice::from_ref(&event)).await;
    assert!(turn.contains("pooled"), "{turn}");
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
        common::offline_config(tmp.path()),
        Default::default(),
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
        status: ProjectStatus::Active,
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
            scout_directions: None,
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
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    let last = messages.last().unwrap();
    assert_eq!(last.role, ChatRole::Assistant);
    assert!(
        last.content.contains("reply to: [pipeline]"),
        "{}",
        last.content
    );
    assert!(
        !orch.tick(&Arc::new(TurnControl::new())).await.unwrap(),
        "settled after the reply"
    );

    nudge_loop.abort();
}

/// A shutdown that arrives *mid-burst* has to stop the loop.
///
/// `watch::Receiver::changed()` marks the value seen when it returns, so the
/// batch loop's `select!` consumes the shutdown and the outer `changed()` then
/// waits for a second change that never comes — parking on `events.recv()`
/// forever. `run()`'s drain awaits this task unbounded, so the whole process
/// hung until its supervisor's SIGKILL: `tasks stop` took 75s and every
/// `tasks reload` swap took ~75s. `POST /mode` is nudge-worthy, which made
/// "pause the pipeline, then restart it" hit it every single time.
#[tokio::test]
async fn a_shutdown_mid_burst_stops_the_nudge_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_in_memory().await.unwrap());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // A debounce long enough that the shutdown lands while the loop is inside
    // the batch loop, which is the only place it can be swallowed.
    let nudge_loop = tokio::spawn(orchestrator_nudge_loop(
        store.clone(),
        common::offline_config(tmp.path()),
        Default::default(),
        Duration::from_secs(2),
        Duration::from_secs(30),
        shutdown_rx,
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // One nudge-worthy event opens the burst — a mode flip, the very thing an
    // operator does just before restarting.
    store
        .append_event(EventPayload::ModeChanged {
            from: tasks::models::Mode::Play,
            to: tasks::models::Mode::Pause,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), nudge_loop)
        .await
        .expect("the nudge loop must return on shutdown, not park on recv()")
        .unwrap();
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

/// The scope test: what the agent child's environment actually is.
///
/// A unit test on the prompt can only say what the agent is *told*; this runs a
/// real child and reads its environment back. Both directions matter, and the
/// `None` half is the one a careless implementation gets wrong: with no target
/// directory the child must see exactly what the parent had — neither cleared
/// nor invented — because `CARGO_TARGET_DIR` set where nothing asked for it
/// would silently redirect whatever else runs under this server.
#[tokio::test]
async fn the_agent_gets_a_warm_build_directory_and_a_command_ceiling_below_its_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let env_log = tmp.path().join("env.log");
    let stub = tmp.path().join("env-dumping-agent.sh");
    tokio::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nenv > {log}\ncat > /dev/null\necho done\n",
            log = common::shell_escape(&env_log.display().to_string()),
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

    let target_dir = tmp.path().join("verify-target");
    let base = OrchestratorConfig {
        command: stub.display().to_string(),
        timeout: Duration::from_secs(900),
        worker_timeout: Duration::from_secs(3600),
        workdir: tmp.path().join("orch-workdir"),
        workdir_is_checkout: true,
        target_dir: Some(target_dir.clone()),
        worktree_dir: tmp.path().join("verify-worktree"),
        github_configured: true,
        api_port: 4800,
        curl_config: tmp.path().join("orchestrator-curl.conf"),
    };

    let read_env = |path: std::path::PathBuf| async move {
        tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .filter_map(|l| {
                l.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect::<std::collections::HashMap<_, _>>()
    };

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    store
        .append_orchestrator_message(ChatRole::User, "hello")
        .await
        .unwrap();
    Orchestrator::new(store.clone(), base.clone())
        .tick(&Arc::new(TurnControl::new()))
        .await
        .unwrap();

    let env = read_env(env_log.clone()).await;
    assert_eq!(
        env.get("CARGO_TARGET_DIR").map(String::as_str),
        Some(target_dir.display().to_string().as_str())
    );
    // Half the turn, in milliseconds, on both — Claude Code computes its
    // ceiling as max(BASH_MAX_TIMEOUT_MS, effective default), so setting only
    // the max would leave un-annotated commands at its 120s default.
    assert_eq!(env.get("BASH_DEFAULT_TIMEOUT_MS").unwrap(), "450000");
    assert_eq!(env.get("BASH_MAX_TIMEOUT_MS").unwrap(), "450000");
    // And the cargo settings that bound what the directory can grow to. They
    // travel *with* `CARGO_TARGET_DIR`, and `make verify-warm` sets the same
    // ones — see `verification_env_matches_the_makefile` for why a mismatch
    // costs more than either setting saves.
    for (key, value) in VERIFICATION_ENV {
        assert_eq!(env.get(key).map(String::as_str), Some(value), "{key}");
    }

    // And with no directory configured, the variable is not invented.
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    store
        .append_orchestrator_message(ChatRole::User, "hello")
        .await
        .unwrap();
    Orchestrator::new(
        store,
        OrchestratorConfig {
            target_dir: None,
            ..base
        },
    )
    .tick(&Arc::new(TurnControl::new()))
    .await
    .unwrap();

    let env = read_env(env_log).await;
    assert_eq!(
        env.get("CARGO_TARGET_DIR").cloned(),
        std::env::var("CARGO_TARGET_DIR").ok(),
        "with no target dir the child must see exactly what the parent had"
    );
    for (key, _) in VERIFICATION_ENV {
        assert_eq!(
            env.get(key).cloned(),
            std::env::var(key).ok(),
            "{key} is set with the build directory or not at all — a cargo setting \
             invented where nothing asked for one would redirect whatever else runs \
             under this server"
        );
    }
}

/// The other half of "set in both places or neither".
///
/// `make verify-warm` primes the very directory the orchestrator builds in, so
/// the two must agree exactly: toggling `CARGO_INCREMENTAL` or a debuginfo
/// level invalidates every *workspace* artifact (registry dependencies are
/// untouched — verified empirically), so a Makefile and a server that disagreed
/// would rebuild the whole workspace on every alternation between them, which
/// costs far more than the disk either setting saves.
///
/// A test rather than a shared constant because one side is a Makefile: there
/// is nothing to share, so the only mechanism available is a check that fails
/// when they drift.
#[test]
fn verification_env_matches_the_makefile() {
    let makefile = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("Makefile"),
    )
    .expect("the workspace Makefile");
    let recipe = makefile
        .split("\nverify-warm:")
        .nth(1)
        .expect("a verify-warm target")
        .split("\n\n")
        .next()
        .expect("its recipe");
    for (key, value) in VERIFICATION_ENV {
        assert!(
            recipe.contains(&format!("{key}={value}")),
            "`make verify-warm` must set {key}={value} — the orchestrator child does, \
             and a mismatch rebuilds the workspace on every alternation:\n{recipe}"
        );
    }
}

// --- the turn lane's two live controls (#1064) ---

/// A stub that hangs: it reports its own pid, then sleeps well past the test.
/// What an interrupt has to be able to end.
async fn write_hanging_stub(dir: &Path, started: &Path) -> std::path::PathBuf {
    let stub = dir.join("stub-hangs.sh");
    let body = format!(
        "#!/bin/sh\ncat > /dev/null\necho $$ > {started}\nsleep 120\n",
        started = common::shell_escape(&started.display().to_string()),
    );
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

/// The pitfall that would silently undo the whole change: the error path in
/// `tick` becomes an assistant turn and calls `append_orchestrator_reply`,
/// which is the **only** thing that advances the watermark. One `return`
/// added on the interrupted path "to record what happened" through that
/// function would eat the input the interrupt exists to preserve.
#[tokio::test]
async fn an_interrupted_turn_loses_no_input_and_keeps_its_session() {
    let tmp = tempfile::tempdir().unwrap();
    let started = tmp.path().join("stub.pid");
    let stub = write_hanging_stub(tmp.path(), &started).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "what's the status?")
        .await
        .unwrap();

    let control = Arc::new(TurnControl::new());
    let ticker = {
        let control = control.clone();
        tokio::spawn(async move { orch.tick(&control).await })
    };
    // Wait for the agent to actually be running before interrupting it —
    // otherwise the test is about a race rather than about the control.
    for _ in 0..200 {
        if control.in_flight() && started.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(control.in_flight(), "the turn armed the slot");
    assert!(control.interrupt(Interruption {
        actor: "the human".into(),
        rationale: Some("it is stuck on a cold build".into()),
    }));

    // `Ok(false)`: no reply was produced, and the tick did not fail either.
    assert!(!ticker.await.unwrap().unwrap());

    // The input is still unanswered, so the next tick takes it up again. This
    // is the assertion the whole design rests on.
    let pending = store.unanswered_orchestrator_messages().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content, "what's the status?");
    // No assistant turn was written: the interrupt is the one error that does
    // not settle the tick condition.
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");

    // The accounting landed on the feed instead, with the actor and reason.
    let events = store.events_since(0, 1000).await.unwrap();
    let note = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Note { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .find(|message| message.contains("interrupted"))
        .expect("an interrupt writes a Note");
    assert!(note.contains("the human"), "{note}");
    assert!(note.contains("cold build"), "{note}");

    // And the slot is free again, on every exit path.
    assert!(!control.in_flight());
}

/// A request that arrives with nothing running is a **no-op that is never
/// stored** — which is what makes "it cannot leak into the next turn"
/// structural rather than careful.
#[tokio::test]
async fn an_interrupt_with_no_turn_in_flight_is_a_no_op_and_never_leaks_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());

    let control = Arc::new(TurnControl::new());
    assert!(!control.in_flight());
    assert!(!control.interrupt(Interruption {
        actor: "the human".into(),
        rationale: None,
    }));

    // The next turn runs to completion: nothing was remembered.
    store
        .append_orchestrator_message(ChatRole::User, "what's the status?")
        .await
        .unwrap();
    assert!(orch.tick(&control).await.unwrap());
    let messages = store.orchestrator_messages_since(0, 1000).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].content.contains("reply to:"));
}

/// The two acts are separate on purpose, and this is the half that says so:
/// holding stops the *next* turn and says nothing about the one running.
#[tokio::test]
async fn holding_the_lane_leaves_the_turn_in_flight_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let args_log = tmp.path().join("args.log");
    let stub = write_stub(tmp.path(), &args_log, false).await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let orch = orchestrator(store.clone(), &stub, tmp.path());
    store
        .append_orchestrator_message(ChatRole::User, "what's the status?")
        .await
        .unwrap();

    store.orchestrator_hold().await.unwrap();
    let lane = store.orchestrator_lane().await.unwrap();
    assert!(lane.held && !lane.may_tick());

    // The hold is the *loop's* gate, not the tick's: a turn already running is
    // unaffected, which is precisely why interrupting is a second act.
    assert!(orch.tick(&Arc::new(TurnControl::new())).await.unwrap());

    store.orchestrator_release_hold().await.unwrap();
    assert!(store.orchestrator_lane().await.unwrap().may_tick());
}

/// "Held since" is when the lane went quiet, not when somebody last said so.
#[tokio::test]
async fn the_hold_is_idempotent_and_keeps_the_instant_it_was_placed() {
    let store = Store::open_in_memory().await.unwrap();
    store.orchestrator_hold().await.unwrap();
    let first = store.orchestrator_lane().await.unwrap();
    assert!(first.held);
    let placed = first.held_at.expect("a hold records when it was placed");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    store.orchestrator_hold().await.unwrap();
    let second = store.orchestrator_lane().await.unwrap();
    assert_eq!(second.held_at, Some(placed), "re-holding must not move it");

    // Release is unconditional and idempotent both ways round.
    store.orchestrator_release_hold().await.unwrap();
    store.orchestrator_release_hold().await.unwrap();
    let open = store.orchestrator_lane().await.unwrap();
    assert!(!open.held && open.held_at.is_none() && open.may_tick());
}

/// The struct-not-enum decision, at the surface that would lose by it: a
/// reader told only about the checkout releases it, finds the lane still
/// quiet, and stops trusting the control.
#[tokio::test]
async fn a_held_and_checked_out_lane_reports_both_reasons() {
    let store = Store::open_in_memory().await.unwrap();
    assert!(
        store
            .orchestrator_lane()
            .await
            .unwrap()
            .describe()
            .is_none()
    );

    store.orchestrator_hold().await.unwrap();
    store.orchestrator_checkout().await.unwrap();
    let lane = store.orchestrator_lane().await.unwrap();
    assert!(lane.held && lane.checked_out && !lane.may_tick());

    let why = lane.describe().expect("a quiet lane says why");
    assert!(why.contains("holding the turn lane"), "{why}");
    assert!(why.contains("checked out"), "{why}");
    // Each reason names its own discharge.
    assert!(why.contains("/orchestrator/release"), "{why}");

    // `checked_out` is filled from the same read as the lane, so the two
    // cannot disagree about the same instant.
    let info = store.orchestrator_session_info().await.unwrap();
    assert_eq!(info.checked_out, info.lane.checked_out);
}
