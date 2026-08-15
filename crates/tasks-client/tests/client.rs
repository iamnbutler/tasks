//! Integration tests against the real server: real router, real in-memory
//! SQLite store, real TCP on an OS-assigned port (repo convention — no
//! mocks). The async server runs on a tokio runtime held by the test; the
//! blocking client calls it from the test thread like any GUI worker would.

use std::sync::Arc;

use chrono::Utc;
use tasks::server::router;
use tasks::store::Store;
use tasks_api::events::EventPayload;
use tasks_api::models::{
    GhState, Mode, Project, Session, SessionId, SessionStatus, Task, TaskId, TaskState,
    TranscriptStream,
};
use tasks_client::{Client, ClientError, EventStreamItem};

struct TestServer {
    client: Client,
    store: Arc<Store>,
    /// Keeps the server's executor alive for the test's duration.
    runtime: tokio::runtime::Runtime,
}

fn spawn_server() -> TestServer {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (store, addr) = runtime.block_on(async {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve_store = store.clone();
        tokio::spawn(async move {
            axum::serve(listener, router(serve_store)).await.unwrap();
        });
        (store, addr)
    });
    TestServer {
        client: Client::with_base(format!("http://{addr}")),
        store,
        runtime,
    }
}

impl TestServer {
    fn seed_task(&self, project: &Project, number: u64) -> Task {
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: format!("task {number}"),
            body: "body".into(),
            labels: vec![],
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
        };
        self.runtime
            .block_on(self.store.insert_task(&task))
            .unwrap();
        task
    }
}

#[test]
fn projects_round_trip() {
    let server = spawn_server();
    let created = server.client.create_project("iamnbutler", "tasks").unwrap();
    assert_eq!(created.repo_owner, "iamnbutler");

    let listed = server.client.projects().unwrap();
    assert_eq!(listed, vec![created]);
}

#[test]
fn api_errors_carry_the_servers_message() {
    let server = spawn_server();

    // 404 with the server's own phrasing.
    let err = server
        .client
        .task(&TaskId::from_raw("task_missing"))
        .unwrap_err();
    match err {
        ClientError::Api { status, message } => {
            assert_eq!(status, 404);
            assert!(message.contains("task_missing"), "message: {message}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }

    // 400: validation text comes through the {"error"} body.
    let err = server.client.create_project("", "").unwrap_err();
    match err {
        ClientError::Api { status, message } => {
            assert_eq!(status, 400);
            assert!(message.contains("non-empty"), "message: {message}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[test]
fn queue_flow_round_trips_typed_states() {
    let server = spawn_server();
    let project = server.client.create_project("iamnbutler", "tasks").unwrap();
    let task = server.seed_task(&project, 7);

    let queued = server.client.queue_task(&task.id).unwrap();
    assert_eq!(queued.state, TaskState::Queued);
    assert_eq!(queued.manual_rank, Some(1));

    let reordered = server.client.reorder_queue(vec![task.id.clone()]).unwrap();
    assert_eq!(reordered.len(), 1);

    let back = server.client.dequeue_task(&task.id).unwrap();
    assert_eq!(back.state, TaskState::Backlog);
    assert_eq!(back.manual_rank, None);
}

#[test]
fn mode_round_trip() {
    let server = spawn_server();
    // A fresh store starts paused — going live is a human decision.
    assert_eq!(server.client.mode().unwrap(), Mode::Pause);
    assert_eq!(server.client.set_mode(Mode::Play).unwrap(), Mode::Play);
    assert_eq!(server.client.mode().unwrap(), Mode::Play);
}

#[test]
fn event_stream_connects_then_delivers_typed_events() {
    let server = spawn_server();
    let mut stream = server.client.stream_events();

    // Connected fires before any event from the connection — the signal to
    // snapshot. It must arrive before we cause the first event.
    match stream.next() {
        Some(EventStreamItem::Connected) => {}
        other => panic!("expected Connected first, got {other:?}"),
    }

    let project = server.client.create_project("iamnbutler", "tasks").unwrap();

    match stream.next() {
        Some(EventStreamItem::Event(event)) => {
            assert!(event.seq > 0);
            match event.payload {
                EventPayload::ProjectAdded { project_id } => {
                    assert_eq!(project_id, project.id);
                }
                other => panic!("expected project_added, got {other:?}"),
            }
            assert_eq!(stream.last_seq(), event.seq);
        }
        other => panic!("expected the project_added event, got {other:?}"),
    }
}

#[test]
fn transcript_tail_replays_then_follows() {
    let server = spawn_server();
    let project = server.client.create_project("iamnbutler", "tasks").unwrap();
    let task = server.seed_task(&project, 9);

    let session = Session {
        id: SessionId::new(),
        task_id: task.id.clone(),
        vm_id: None,
        branch: "scout/9".into(),
        status: SessionStatus::Running,
        started_at: Utc::now(),
        completed_at: None,
        exit_reason: None,
        usage: None,
    };
    server
        .runtime
        .block_on(server.store.insert_session(&session))
        .unwrap();
    server
        .runtime
        .block_on(server.store.append_transcript_lines(
            &session.id,
            &[
                (TranscriptStream::Stdout, "first".into()),
                (TranscriptStream::Stderr, "second".into()),
            ],
        ))
        .unwrap();

    let mut tail = server.client.stream_transcript(&session.id, 0);

    // Replay: the two persisted lines, in order, typed.
    let first = tail.next().unwrap().unwrap();
    assert_eq!((first.seq, first.line.as_str()), (1, "first"));
    assert_eq!(first.stream, TranscriptStream::Stdout);
    let second = tail.next().unwrap().unwrap();
    assert_eq!((second.seq, second.line.as_str()), (2, "second"));
    assert_eq!(second.stream, TranscriptStream::Stderr);

    // Live: a line appended after the tail attached still arrives.
    server
        .runtime
        .block_on(
            server.store.append_transcript_lines(
                &session.id,
                &[(TranscriptStream::Stdout, "third".into())],
            ),
        )
        .unwrap();
    let third = tail.next().unwrap().unwrap();
    assert_eq!((third.seq, third.line.as_str()), (3, "third"));
}

#[test]
fn transcript_stream_for_unknown_session_is_terminal() {
    let server = spawn_server();
    let mut tail = server
        .client
        .stream_transcript(&SessionId::from_raw("sess_missing"), 0);

    match tail.next() {
        Some(Err(ClientError::Api { status, .. })) => assert_eq!(status, 404),
        other => panic!("expected a 404, got {other:?}"),
    }
    // Terminal: the iterator ends instead of reconnect-looping on a 404.
    assert!(tail.next().is_none());
}

#[test]
fn orchestrator_message_send_and_list() {
    let server = spawn_server();
    let sent = server
        .client
        .send_orchestrator_message("hello orchestrator")
        .unwrap();
    assert_eq!(sent.content, "hello orchestrator");

    let listed = server.client.orchestrator_messages(0).unwrap();
    assert_eq!(listed, vec![sent]);

    let empty = server.client.send_orchestrator_message("   ").unwrap_err();
    assert!(matches!(empty, ClientError::Api { status: 400, .. }));
}

/// The live feed end to end — the exact path the GUI subscribes on. The feed
/// is ephemeral: no `Connected` item, no backfill, so there is no observable
/// moment the subscription goes live. A publisher thread therefore repeats the
/// whole tick until the reader catches a complete round.
#[test]
fn orchestrator_feed_delivers_a_tick_in_order() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tasks_api::models::OrchestratorFeedEvent as Feed;

    let server = spawn_server();
    let stop = Arc::new(AtomicBool::new(false));
    let publisher = {
        let store = server.store.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                store.publish_orchestrator_feed(Feed::Started);
                store.publish_orchestrator_feed(Feed::Delta {
                    text: "check".into(),
                });
                store.publish_orchestrator_feed(Feed::Tool {
                    label: "Bash: curl -s /tasks".into(),
                });
                store.publish_orchestrator_feed(Feed::Done);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
    };

    let mut feed = server.client.stream_orchestrator();
    // Join mid-round if we have to: a round starts at the first `Started`.
    let mut round = Vec::new();
    while round.len() < 4 {
        let event = feed.next().expect("feed never ends here").unwrap();
        if round.is_empty() && event != Feed::Started {
            continue;
        }
        round.push(event);
    }
    stop.store(true, Ordering::Relaxed);
    publisher.join().unwrap();

    assert_eq!(
        round,
        vec![
            Feed::Started,
            Feed::Delta {
                text: "check".into()
            },
            Feed::Tool {
                label: "Bash: curl -s /tasks".into()
            },
            Feed::Done,
        ]
    );
}
