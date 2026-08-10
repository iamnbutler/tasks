//! Golden JSON fixtures for every shape the HTTP API returns.
//!
//! One file per wire shape lives in `<repo>/fixtures`, committed. This test
//! serializes a deterministic instance of each shape and asserts the bytes
//! match. `UPDATE_FIXTURES=1 cargo test -p tasks --test wire_fixtures` rewrites
//! them, so changing the contract is a conscious, reviewable act rather than a
//! silent one.
//!
//! The point is the *other* consumer: `app/TasksTests/WireFixtureTests.swift`
//! decodes these same files through the app's production decoder. A field
//! rename here fails `cargo test` first, then fails the app's suite until the
//! Swift models catch up.
//!
//! Fixtures live at the repo root, not under this crate: they are a
//! cross-language artifact and this crate is only one of their consumers. See
//! `fixtures/README.md`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use tasks::events::{Event, EventPayload};
use tasks::models::{
    Build, BuildId, BuildStatus, Complexity, GhState, Mode, Project, ProjectId, Session, SessionId,
    SessionStatus, SessionUsage, Spec, SpecId, SpecQueueEntry, SpecQueueItem, SpecQueueStatus,
    Task, TaskId, TaskState, TranscriptLine, TranscriptStream,
};
use tasks::server::{ApiError, BuildDetail, ModeResponse};

/// Number of `EventPayload` variants that must each get a fixture. Bumped
/// deliberately: `kind_of` stops compiling when a variant is added, and this
/// count then fails until the variant has a file of its own.
const EVENT_VARIANT_COUNT: usize = 16;

/// The command a failing assertion tells you to run.
const REGEN: &str = "UPDATE_FIXTURES=1 cargo test -p tasks --test wire_fixtures";

// --- harness ---

struct Fixture {
    name: String,
    json: String,
}

/// Render one fixture.
///
/// Serializing the **typed value** matters: routing through `serde_json::Value`
/// would sort keys alphabetically (its `Map` is a `BTreeMap` unless the
/// `preserve_order` feature is on) and the fixture would stop reflecting the
/// field order the server actually writes. Same trap applies to anything built
/// with `json!`.
fn fixture<T: Serialize>(name: impl Into<String>, value: &T) -> Fixture {
    let name = name.into();
    let json = serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| panic!("fixture {name} does not serialize: {err}"));
    Fixture {
        name,
        json: format!("{json}\n"),
    }
}

fn fixtures_dir() -> PathBuf {
    // crates/tasks -> repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures")
}

fn update_mode() -> bool {
    std::env::var_os("UPDATE_FIXTURES").is_some_and(|v| !v.is_empty() && v != "0")
}

/// A line-oriented diff, because the failure message is the deliverable as much
/// as the assertion is. "assertion failed: left == right" over 400 lines of
/// JSON teaches nobody anything.
fn diff(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for i in 0..expected.len().max(actual.len()) {
        let (e, a) = (expected.get(i), actual.get(i));
        if e != a {
            out.push_str(&format!(
                "    line {}:\n      committed: {}\n      generated: {}\n",
                i + 1,
                e.map(|s| format!("{s:?}")).unwrap_or("<missing>".into()),
                a.map(|s| format!("{s:?}")).unwrap_or("<missing>".into()),
            ));
        }
    }
    out
}

// --- deterministic sample values ---

const PROJECT_ID: &str = "proj_0f4b1c2d3e4f5a6b7c8d9e0f1a2b3c4d";
const TASK_ID: &str = "task_1a2b3c4d5e6f708192a3b4c5d6e7f809";
const TASK_ID_B: &str = "task_2b3c4d5e6f708192a3b4c5d6e7f8091a";
const SESSION_ID: &str = "sess_3c4d5e6f708192a3b4c5d6e7f8091a2b";
const SPEC_ID: &str = "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c";
const SPEC_ID_B: &str = "spec_5e6f708192a3b4c5d6e7f8091a2b3c4d";
const BUILD_ID: &str = "build_6f708192a3b4c5d6e7f8091a2b3c4d5e";

fn at(raw: &str) -> DateTime<Utc> {
    raw.parse()
        .unwrap_or_else(|err| panic!("bad fixture timestamp {raw}: {err}"))
}

fn project() -> Project {
    Project {
        id: ProjectId::from_raw(PROJECT_ID),
        repo_owner: "iamnbutler".into(),
        repo_name: "tasks".into(),
        added_at: at("2026-08-01T09:15:00Z"),
    }
}

/// Every optional field populated and every collection non-empty.
fn task() -> Task {
    Task {
        id: TaskId::from_raw(TASK_ID),
        project_id: ProjectId::from_raw(PROJECT_ID),
        gh_issue_number: 763,
        title: "Golden JSON wire fixtures shared by the Rust API and Swift client tests".into(),
        body: "The Swift models are hand-mirrored from models.rs.\n\nNothing catches drift.".into(),
        labels: vec!["enhancement".into(), "testing".into()],
        gh_state: GhState::Open,
        state: TaskState::InReview,
        priority: 3,
        manual_rank: Some(1),
        dispatch_attempts: 2,
        ingested_at: at("2026-08-02T10:00:00Z"),
        updated_at: at("2026-08-09T18:30:00.123Z"),
    }
}

/// The other half of the contract: nulls and empty collections, pinned so a
/// client can't quietly assume the populated path is the only one.
fn task_minimal() -> Task {
    Task {
        id: TaskId::from_raw(TASK_ID_B),
        project_id: ProjectId::from_raw(PROJECT_ID),
        gh_issue_number: 764,
        title: "Untriaged issue".into(),
        body: String::new(),
        labels: vec![],
        gh_state: GhState::Closed,
        state: TaskState::Backlog,
        priority: 0,
        manual_rank: None,
        dispatch_attempts: 0,
        ingested_at: at("2026-08-02T10:05:00Z"),
        updated_at: at("2026-08-02T10:05:00Z"),
    }
}

fn session() -> Session {
    Session {
        id: SessionId::from_raw(SESSION_ID),
        task_id: TaskId::from_raw(TASK_ID),
        vm_id: Some("vm_7c1f9a4e".into()),
        branch: "scout/task_1a2b3c4d".into(),
        status: SessionStatus::ScoutSucceeded,
        started_at: at("2026-08-09T17:00:00Z"),
        completed_at: Some(at("2026-08-09T17:23:41.456Z")),
        exit_reason: Some("spec reported".into()),
        usage: Some(SessionUsage {
            input_tokens: Some(184_320),
            output_tokens: Some(21_004),
            cache_read_input_tokens: Some(1_204_887),
            cache_creation_input_tokens: Some(96_512),
            total_cost_usd: Some(4.2175),
            duration_ms: Some(1_421_000),
            num_turns: Some(87),
        }),
    }
}

/// In flight: no vm yet reported terminal, no completion, no usage.
fn session_running() -> Session {
    Session {
        id: SessionId::from_raw(SESSION_ID),
        task_id: TaskId::from_raw(TASK_ID),
        vm_id: None,
        branch: "scout/task_1a2b3c4d".into(),
        status: SessionStatus::Running,
        started_at: at("2026-08-09T17:00:00Z"),
        completed_at: None,
        exit_reason: None,
        usage: None,
    }
}

fn spec() -> Spec {
    Spec {
        id: SpecId::from_raw(SPEC_ID),
        session_id: SessionId::from_raw(SESSION_ID),
        task_id: TaskId::from_raw(TASK_ID),
        content: "## Spec: Golden JSON wire fixtures\n\n### Summary\n\nCommit one JSON file per wire shape.\n".into(),
        complexity: Complexity::Medium,
        files_touched: vec![
            "crates/tasks/tests/wire_fixtures.rs".into(),
            "app/TasksTests/WireFixtureTests.swift".into(),
        ],
        created_at: at("2026-08-09T17:23:40Z"),
    }
}

/// `SpecQueueItem` uses `#[serde(flatten)]`, and flatten preserves declaration
/// order through `serialize_struct` — so this is flat, with `task_id` last, not
/// a nested `entry` object. Worth knowing before anyone "fixes" a client model
/// to expect nesting.
fn spec_queue_item() -> SpecQueueItem {
    SpecQueueItem {
        entry: SpecQueueEntry {
            spec_id: SpecId::from_raw(SPEC_ID),
            status: SpecQueueStatus::Approved,
            rank: Some(1),
            approved_at: Some(at("2026-08-09T18:00:00Z")),
            feedback: Some("Good. Ship the fixtures before the models drift further.".into()),
            blocking_dependencies: vec![TaskId::from_raw(TASK_ID_B)],
        },
        task_id: TaskId::from_raw(TASK_ID),
    }
}

fn spec_queue_item_pending() -> SpecQueueItem {
    SpecQueueItem {
        entry: SpecQueueEntry {
            spec_id: SpecId::from_raw(SPEC_ID_B),
            status: SpecQueueStatus::PendingReview,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        },
        task_id: TaskId::from_raw(TASK_ID_B),
    }
}

fn build() -> Build {
    Build {
        id: BuildId::from_raw(BUILD_ID),
        project_id: ProjectId::from_raw(PROJECT_ID),
        vm_id: Some("vm_2e8b3d17".into()),
        branch: "build/build_6f708192".into(),
        base_branch: "main".into(),
        base_sha: Some("a3b1d0c9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3".into()),
        head_sha: Some("b4c2e1d0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4".into()),
        pr_number: Some(781),
        status: BuildStatus::Succeeded,
        summary: Some("Adds golden wire fixtures and the tests on both sides of them.".into()),
        files_touched: vec!["fixtures/task.json".into(), "docs/clients.md".into()],
        exit_reason: None,
        created_at: at("2026-08-09T19:00:00Z"),
        started_at: Some(at("2026-08-09T19:00:05Z")),
        completed_at: Some(at("2026-08-09T19:41:12.789Z")),
    }
}

/// Freshly requested: everything the run fills in is still null.
fn build_queued() -> Build {
    Build {
        id: BuildId::from_raw(BUILD_ID),
        project_id: ProjectId::from_raw(PROJECT_ID),
        vm_id: None,
        branch: "build/build_6f708192".into(),
        base_branch: "main".into(),
        base_sha: None,
        head_sha: None,
        pr_number: None,
        status: BuildStatus::Queued,
        summary: None,
        files_touched: vec![],
        exit_reason: None,
        created_at: at("2026-08-09T19:00:00Z"),
        started_at: None,
        completed_at: None,
    }
}

fn transcript_line() -> TranscriptLine {
    TranscriptLine {
        session_id: SessionId::from_raw(SESSION_ID),
        seq: 42,
        timestamp: at("2026-08-09T17:04:09.250Z"),
        stream: TranscriptStream::Stdout,
        line: r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading models.rs"}]}}"#.into(),
    }
}

fn event(seq: i64, payload: EventPayload) -> Event {
    Event {
        seq,
        timestamp: at("2026-08-09T17:00:00.500Z"),
        payload,
    }
}

/// The body `ApiError::into_response` writes. Mirrored as a type here so the
/// fixture renders from a typed value like every other one;
/// `error_fixture_matches_api_error` ties it back to the real handler output.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

// --- enum inventories ---
//
// Each function lists every variant and then matches on them exhaustively. The
// match is the forcing function: add a variant in `models.rs` and this file
// stops compiling until the variant is named here — at which point you also
// have to put it in the list above to make the match reachable in practice.

fn all_gh_states() -> Vec<GhState> {
    let all = vec![GhState::Open, GhState::Closed];
    for value in &all {
        match value {
            GhState::Open | GhState::Closed => {}
        }
    }
    all
}

fn all_task_states() -> Vec<TaskState> {
    let all = vec![
        TaskState::Backlog,
        TaskState::Queued,
        TaskState::Scouting,
        TaskState::InReview,
        TaskState::ReadyToBuild,
        TaskState::Building,
        TaskState::Done,
        TaskState::Rejected,
    ];
    for value in &all {
        match value {
            TaskState::Backlog
            | TaskState::Queued
            | TaskState::Scouting
            | TaskState::InReview
            | TaskState::ReadyToBuild
            | TaskState::Building
            | TaskState::Done
            | TaskState::Rejected => {}
        }
    }
    all
}

fn all_session_statuses() -> Vec<SessionStatus> {
    let all = vec![
        SessionStatus::Running,
        SessionStatus::ScoutSucceeded,
        SessionStatus::ScoutFailed,
        SessionStatus::Cancelled,
    ];
    for value in &all {
        match value {
            SessionStatus::Running
            | SessionStatus::ScoutSucceeded
            | SessionStatus::ScoutFailed
            | SessionStatus::Cancelled => {}
        }
    }
    all
}

fn all_spec_queue_statuses() -> Vec<SpecQueueStatus> {
    let all = vec![
        SpecQueueStatus::PendingReview,
        SpecQueueStatus::Approved,
        SpecQueueStatus::NeedsRevision,
        SpecQueueStatus::Blocked,
        SpecQueueStatus::Rejected,
        SpecQueueStatus::Built,
    ];
    for value in &all {
        match value {
            SpecQueueStatus::PendingReview
            | SpecQueueStatus::Approved
            | SpecQueueStatus::NeedsRevision
            | SpecQueueStatus::Blocked
            | SpecQueueStatus::Rejected
            | SpecQueueStatus::Built => {}
        }
    }
    all
}

fn all_build_statuses() -> Vec<BuildStatus> {
    let all = vec![
        BuildStatus::Queued,
        BuildStatus::Running,
        BuildStatus::Succeeded,
        BuildStatus::Failed,
    ];
    for value in &all {
        match value {
            BuildStatus::Queued
            | BuildStatus::Running
            | BuildStatus::Succeeded
            | BuildStatus::Failed => {}
        }
    }
    all
}

fn all_complexities() -> Vec<Complexity> {
    let all = vec![Complexity::Simple, Complexity::Medium, Complexity::Complex];
    for value in &all {
        match value {
            Complexity::Simple | Complexity::Medium | Complexity::Complex => {}
        }
    }
    all
}

fn all_modes() -> Vec<Mode> {
    let all = vec![Mode::Play, Mode::Pause, Mode::Stop];
    for value in &all {
        match value {
            Mode::Play | Mode::Pause | Mode::Stop => {}
        }
    }
    all
}

fn all_transcript_streams() -> Vec<TranscriptStream> {
    let all = vec![TranscriptStream::Stdout, TranscriptStream::Stderr];
    for value in &all {
        match value {
            TranscriptStream::Stdout | TranscriptStream::Stderr => {}
        }
    }
    all
}

/// The wire `kind` of an event payload.
///
/// Exhaustive on purpose: a new `EventPayload` variant breaks this file, and
/// then [`EVENT_VARIANT_COUNT`] keeps failing until the variant has a fixture.
fn kind_of(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::ProjectAdded { .. } => "project_added",
        EventPayload::TaskIngested { .. } => "task_ingested",
        EventPayload::TaskStateChanged { .. } => "task_state_changed",
        EventPayload::TaskGhStateChanged { .. } => "task_gh_state_changed",
        EventPayload::SessionStarted { .. } => "session_started",
        EventPayload::SessionCompleted { .. } => "session_completed",
        EventPayload::SpecCreated { .. } => "spec_created",
        EventPayload::SpecQueueStatusChanged { .. } => "spec_queue_status_changed",
        EventPayload::QueueReordered { .. } => "queue_reordered",
        EventPayload::SpecQueueReordered { .. } => "spec_queue_reordered",
        EventPayload::BuildRequested { .. } => "build_requested",
        EventPayload::BuildStarted { .. } => "build_started",
        EventPayload::BuildCompleted { .. } => "build_completed",
        EventPayload::PullRequestOpened { .. } => "pull_request_opened",
        EventPayload::ModeChanged { .. } => "mode_changed",
        EventPayload::Note { .. } => "note",
    }
}

/// One `Event` fixture per payload variant, plus the `from: null` shape of
/// `spec_queue_status_changed` — the only optional field in any payload.
fn event_payloads() -> Vec<(&'static str, EventPayload)> {
    vec![
        (
            "project_added",
            EventPayload::ProjectAdded {
                project_id: ProjectId::from_raw(PROJECT_ID),
            },
        ),
        (
            "task_ingested",
            EventPayload::TaskIngested {
                task_id: TaskId::from_raw(TASK_ID),
                project_id: ProjectId::from_raw(PROJECT_ID),
            },
        ),
        (
            "task_state_changed",
            EventPayload::TaskStateChanged {
                task_id: TaskId::from_raw(TASK_ID),
                from: TaskState::Queued,
                to: TaskState::Scouting,
            },
        ),
        (
            "task_gh_state_changed",
            EventPayload::TaskGhStateChanged {
                task_id: TaskId::from_raw(TASK_ID),
                gh_state: GhState::Closed,
            },
        ),
        (
            "session_started",
            EventPayload::SessionStarted {
                session_id: SessionId::from_raw(SESSION_ID),
                task_id: TaskId::from_raw(TASK_ID),
            },
        ),
        (
            "session_completed",
            EventPayload::SessionCompleted {
                session_id: SessionId::from_raw(SESSION_ID),
                task_id: TaskId::from_raw(TASK_ID),
                status: SessionStatus::ScoutSucceeded,
            },
        ),
        (
            "spec_created",
            EventPayload::SpecCreated {
                spec_id: SpecId::from_raw(SPEC_ID),
                task_id: TaskId::from_raw(TASK_ID),
                session_id: SessionId::from_raw(SESSION_ID),
            },
        ),
        (
            "spec_queue_status_changed",
            EventPayload::SpecQueueStatusChanged {
                spec_id: SpecId::from_raw(SPEC_ID),
                from: Some(SpecQueueStatus::PendingReview),
                to: SpecQueueStatus::Approved,
            },
        ),
        // The entry's first appearance in the queue has no prior status.
        (
            "spec_queue_status_changed_initial",
            EventPayload::SpecQueueStatusChanged {
                spec_id: SpecId::from_raw(SPEC_ID),
                from: None,
                to: SpecQueueStatus::PendingReview,
            },
        ),
        (
            "queue_reordered",
            EventPayload::QueueReordered {
                task_ids: vec![TaskId::from_raw(TASK_ID), TaskId::from_raw(TASK_ID_B)],
            },
        ),
        (
            "spec_queue_reordered",
            EventPayload::SpecQueueReordered {
                spec_ids: vec![SpecId::from_raw(SPEC_ID), SpecId::from_raw(SPEC_ID_B)],
            },
        ),
        (
            "build_requested",
            EventPayload::BuildRequested {
                build_id: BuildId::from_raw(BUILD_ID),
                spec_ids: vec![SpecId::from_raw(SPEC_ID), SpecId::from_raw(SPEC_ID_B)],
            },
        ),
        (
            "build_started",
            EventPayload::BuildStarted {
                build_id: BuildId::from_raw(BUILD_ID),
            },
        ),
        (
            "build_completed",
            EventPayload::BuildCompleted {
                build_id: BuildId::from_raw(BUILD_ID),
                status: BuildStatus::Succeeded,
            },
        ),
        (
            "pull_request_opened",
            EventPayload::PullRequestOpened {
                build_id: BuildId::from_raw(BUILD_ID),
                pr_number: 781,
            },
        ),
        (
            "mode_changed",
            EventPayload::ModeChanged {
                from: Mode::Pause,
                to: Mode::Play,
            },
        ),
        (
            "note",
            EventPayload::Note {
                source: "scout-dispatcher".into(),
                message: "task task_1a2b3c4d rejected after 3 failed dispatches".into(),
            },
        ),
    ]
}

// --- the fixture set ---

fn all_fixtures() -> Vec<Fixture> {
    let mut out = vec![
        fixture("project", &project()),
        fixture("task", &task()),
        fixture("task_minimal", &task_minimal()),
        fixture("task_list", &vec![task(), task_minimal()]),
        fixture("session", &session()),
        fixture("session_running", &session_running()),
        fixture("transcript_line", &transcript_line()),
        fixture("spec", &spec()),
        fixture("spec_queue_item", &spec_queue_item()),
        fixture("spec_queue_item_pending", &spec_queue_item_pending()),
        fixture("build", &build()),
        fixture("build_queued", &build_queued()),
        fixture(
            "build_detail",
            &BuildDetail {
                build: build(),
                spec_ids: vec![SpecId::from_raw(SPEC_ID), SpecId::from_raw(SPEC_ID_B)],
            },
        ),
        fixture("mode_response", &ModeResponse { mode: Mode::Play }),
        fixture(
            "error",
            &ErrorBody {
                error: format!("task {TASK_ID}"),
            },
        ),
        fixture("enums", &enums()),
        fixture("timestamps", &timestamps()),
    ];

    for (seq, (name, payload)) in event_payloads().into_iter().enumerate() {
        out.push(fixture(
            format!("event_{name}"),
            &event(seq as i64 + 1, payload),
        ));
    }

    out
}

/// Every snake_case enum value the API can emit, by enum. A client that keeps a
/// mirrored enum can diff against this file directly.
///
/// This one and [`timestamps`] are the only fixtures built with `json!`, and so
/// the only ones whose top-level keys come out alphabetized. That's fine here:
/// they are inventories keyed by name, not shapes the server sends, so nothing
/// depends on their key order. Every actual wire shape renders from its typed
/// value — see [`fixture`].
fn enums() -> serde_json::Value {
    json!({
        "gh_state": all_gh_states(),
        "task_state": all_task_states(),
        "session_status": all_session_statuses(),
        "transcript_stream": all_transcript_streams(),
        "spec_queue_status": all_spec_queue_statuses(),
        "build_status": all_build_statuses(),
        "complexity": all_complexities(),
        "mode": all_modes(),
    })
}

/// chrono's fractional-second width is value-dependent, not
/// configuration-dependent: it prints 0, 3, 6 or 9 digits and nothing between.
/// A client's date parser has to handle all four, so all four are pinned here.
fn timestamps() -> serde_json::Value {
    json!({
        "whole_seconds": at("2026-08-09T12:00:00Z"),
        "milliseconds": at("2026-08-09T12:00:00.123Z"),
        "microseconds": at("2026-08-09T12:00:00.123456Z"),
        "nanoseconds": at("2026-08-09T12:00:00.123456789Z"),
    })
}

// --- tests ---

#[test]
fn fixtures_match_wire_shapes() {
    let dir = fixtures_dir();
    let fixtures = all_fixtures();

    if update_mode() {
        std::fs::create_dir_all(&dir).expect("create fixtures dir");
        for f in &fixtures {
            std::fs::write(dir.join(format!("{}.json", f.name)), &f.json)
                .unwrap_or_else(|err| panic!("write {}.json: {err}", f.name));
        }
        eprintln!("wrote {} fixtures to {}", fixtures.len(), dir.display());
        return;
    }

    // Collect every mismatch before failing — one run should tell you the whole
    // story, not the first line of it.
    let mut problems = Vec::new();
    for f in &fixtures {
        let path = dir.join(format!("{}.json", f.name));
        match std::fs::read_to_string(&path) {
            Err(err) => problems.push(format!("  {}.json: cannot read ({err})", f.name)),
            Ok(committed) if committed != f.json => problems.push(format!(
                "  {}.json: does not match what the server would serialize\n{}",
                f.name,
                diff(&committed, &f.json)
            )),
            Ok(_) => {}
        }
    }

    assert!(
        problems.is_empty(),
        "{} of {} wire fixtures are stale:\n{}\n\
         If the contract change is intentional, regenerate and review the diff:\n\
         \n    {REGEN}\n\n\
         Then update app/TasksTests/WireFixtureTests.swift (and any other client) to match.",
        problems.len(),
        fixtures.len(),
        problems.join("\n"),
    );
}

/// A `.json` file nothing generates is a shape that was renamed or deleted on
/// the Rust side while a client kept reading the stale copy.
#[test]
fn fixtures_dir_has_no_orphans() {
    let dir = fixtures_dir();
    // Races with `fixtures_match_wire_shapes` creating the directory on a
    // first-ever UPDATE_FIXTURES run; nothing to check until it exists.
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let on_disk: BTreeSet<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    let generated: BTreeSet<String> = all_fixtures()
        .iter()
        .map(|f| format!("{}.json", f.name))
        .collect();

    let orphans: Vec<&String> = on_disk.difference(&generated).collect();
    assert!(
        orphans.is_empty(),
        "{} fixture file(s) in {} are not generated by any wire shape: {:?}\n\
         Delete them, or restore the shape that produced them. Regenerating with\n\
         \n    {REGEN}\n\n\
         does not remove files.",
        orphans.len(),
        dir.display(),
        orphans,
    );
}

/// Every `EventPayload` variant gets its own file, so a client can't cover
/// eleven of sixteen and look complete.
#[test]
fn every_event_variant_has_a_fixture() {
    let kinds: BTreeSet<&'static str> = event_payloads()
        .iter()
        .map(|(_, payload)| kind_of(payload))
        .collect();

    assert_eq!(
        kinds.len(),
        EVENT_VARIANT_COUNT,
        "expected a fixture for each of the {EVENT_VARIANT_COUNT} EventPayload variants, got {}: \
         {kinds:?}\nAdd one to `event_payloads()` and bump EVENT_VARIANT_COUNT.",
        kinds.len(),
    );
}

/// The enum inventories have to be inventories: a duplicated variant in one of
/// the `all_*` lists would otherwise pad `enums.json` and hide a missing value.
#[test]
fn enum_inventories_are_distinct() {
    let inventory = enums();
    for (name, values) in inventory.as_object().expect("enums is an object") {
        let values = values.as_array().expect("enum inventory is an array");
        let unique: BTreeSet<&str> = values.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            unique.len(),
            values.len(),
            "{name} lists a value twice: {values:?}",
        );
    }
}

/// Ties `fixtures/error.json` to the actual handler output rather than a
/// hand-written guess. Compared as parsed JSON, not bytes: axum writes the body
/// compactly and the fixture is pretty-printed for review.
#[tokio::test]
async fn error_fixture_matches_api_error() {
    let response = ApiError::NotFound(format!("task {TASK_ID}")).into_response();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);

    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("collect error body");
    let served: serde_json::Value = serde_json::from_slice(&bytes).expect("error body is JSON");

    let fixture = all_fixtures()
        .into_iter()
        .find(|f| f.name == "error")
        .expect("error fixture exists");
    let expected: serde_json::Value =
        serde_json::from_str(&fixture.json).expect("error fixture is JSON");

    assert_eq!(
        served, expected,
        "the error body the API actually serves has drifted from fixtures/error.json",
    );
}

/// The premise the client date tests rest on: chrono emits exactly these four
/// fractional widths, `Z`-suffixed, and round-trips each one.
#[test]
fn chrono_emits_the_fractional_widths_the_fixtures_claim() {
    let cases = [
        ("2026-08-09T12:00:00Z", 0),
        ("2026-08-09T12:00:00.123Z", 3),
        ("2026-08-09T12:00:00.123456Z", 6),
        ("2026-08-09T12:00:00.123456789Z", 9),
    ];

    for (raw, digits) in cases {
        let parsed = at(raw);
        let rendered = serde_json::to_string(&parsed).expect("serialize timestamp");
        assert_eq!(
            rendered,
            format!("\"{raw}\""),
            "chrono did not round-trip {raw} byte-for-byte",
        );

        let fraction = raw
            .split_once('.')
            .map(|(_, rest)| rest.trim_end_matches('Z').len())
            .unwrap_or(0);
        assert_eq!(
            fraction, digits,
            "{raw} does not have {digits} fractional digits"
        );
    }
}
