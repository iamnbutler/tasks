//! Transcript capture end to end (#759): a real vm-pool service, the real
//! scout-supervisor binary, real SQLite, a real git repo and a stub agent that
//! emits stream-json. No mocks — see CLAUDE.md.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use vm_pool_client::Client;
use vm_pool_protocol::VmConfig;

use tasks::models::{
    GhState, Project, ProjectId, SessionStatus, Task, TaskId, TaskState, TranscriptStream,
};
use tasks::scout::{Scout, ScoutConfig, ScoutTarget};
use tasks::store::Store;
use tasks_protocol::TasksProtocol;

mod common;
use common::{
    make_fixture_repo, spawn_vm_pool, stream_json_agent_path, workspace_bin,
    write_supervisor_wrapper,
};

async fn insert_project_and_task(store: &Store) -> (Project, Task) {
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 1,
        title: "Transcribed task".into(),
        body: "Do the thing".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::Queued,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: now,
        updated_at: now,
    };
    store.insert_task(&task).await.unwrap();
    (project, task)
}

/// The acceptance criterion: a scout run leaves a queryable transcript, and the
/// final `result` record is costed onto the session.
#[tokio::test]
async fn a_scout_run_produces_a_queryable_transcript_and_usage() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        stream_json_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store).await;

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(300),
        },
    );
    let spec = scout
        .dispatch(
            task.clone(),
            &ScoutTarget {
                repo_clone_url: repo_url,
                base_branch: "main".into(),
            },
        )
        .await
        .expect("dispatch");

    let session_id = spec.session_id.clone();
    let lines = store.transcript_since(&session_id, 0, 1000).await.unwrap();
    assert!(!lines.is_empty(), "the run recorded no transcript at all");

    // Dense, 1-based, in order — that is what `?since=` paging relies on.
    assert_eq!(
        lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
        (1..=lines.len() as i64).collect::<Vec<_>>()
    );

    let joined = lines
        .iter()
        .map(|l| l.line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains(r#""type":"assistant""#),
        "stream-json assistant messages missing from:\n{joined}"
    );
    assert!(
        joined.contains(r#""type":"tool_use""#) || joined.contains(r#""name":"Read""#),
        "tool calls missing from the transcript:\n{joined}"
    );
    assert!(
        lines.iter().all(|l| l.session_id == session_id),
        "another session's lines leaked in"
    );
    assert!(
        lines.iter().any(|l| l.stream == TranscriptStream::Stdout),
        "no stdout lines recorded"
    );

    // The final result record is parsed onto the session rather than left in
    // the transcript only.
    let session = store.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::ScoutSucceeded);
    let usage = session.usage.expect("usage parsed from the result record");
    assert_eq!(usage.input_tokens, Some(1200));
    assert_eq!(usage.output_tokens, Some(340));
    assert_eq!(usage.cache_read_input_tokens, Some(880));
    assert_eq!(usage.total_cost_usd, Some(0.0421));
    assert_eq!(usage.num_turns, Some(3));

    // Transcripts are a separate channel: nothing from them may reach the
    // event log, which every client refetches on.
    let events = store.all_events().await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| format!("{:?}", e.payload).contains("tool_use")),
        "transcript content leaked into the event log"
    );
    assert!(
        events.len() < 20,
        "the event log should stay low-rate, got {} events",
        events.len()
    );
}

/// A transcript is complete by the time the session's completion event lands —
/// a client that refetches on `session_completed` must not find a truncated one.
#[tokio::test]
async fn the_transcript_is_complete_before_the_session_completes() {
    let supervisor_bin = workspace_bin("scout-supervisor").await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_fixture_repo(tmp.path(), "fixture-repo").await;
    let repo_url = format!("file://{}", repo.display());
    let workdir_root = tmp.path().join("scout-workdirs");
    tokio::fs::create_dir_all(&workdir_root).await.unwrap();
    let wrapper = write_supervisor_wrapper(
        tmp.path(),
        &supervisor_bin,
        stream_json_agent_path().to_str().unwrap(),
        &workdir_root,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, 2).await;
    let client: Client<TasksProtocol> = Client::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open(tmp.path().join("tasks.db")).await.unwrap());
    let (_project, task) = insert_project_and_task(&store).await;

    let mut events = store.subscribe_events();

    let scout = Scout::new(
        store.clone(),
        client.handle(),
        ScoutConfig {
            image: "agent:v1".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(300),
        },
    );
    let spec = scout
        .dispatch(
            task.clone(),
            &ScoutTarget {
                repo_clone_url: repo_url,
                base_branch: "main".into(),
            },
        )
        .await
        .expect("dispatch");

    // Find the completion event, then read the transcript: everything the run
    // produced must already be readable.
    let mut saw_completion = false;
    while let Ok(event) = events.try_recv() {
        if format!("{:?}", event.payload).contains("SessionCompleted") {
            saw_completion = true;
        }
    }
    assert!(saw_completion, "no session_completed event was appended");

    let lines = store
        .transcript_since(&spec.session_id, 0, 1000)
        .await
        .unwrap();
    assert!(
        lines.iter().any(|l| l.line.contains(r#""type":"result""#)),
        "the final result line was not persisted before completion"
    );
}
