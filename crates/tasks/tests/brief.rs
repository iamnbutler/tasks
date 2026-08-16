//! Computed briefs against a real store.
//!
//! These assert the *facts*, not the phrasing beyond the load-bearing words —
//! a brief exists to make a judgment cheap, so what matters is that the thing
//! a reviewer would have had to go and find is present, and that a check which
//! came back clean cannot be mistaken for a check that never ran.
//!
//! GitHub-derived facts are absent here on purpose: the client has no token,
//! which is the same degraded path a deployment without `GITHUB_TOKEN` runs,
//! and the brief is required to say so rather than shrink quietly.

use std::sync::Arc;

use chrono::Utc;
use tasks::brief::Brief;
use tasks::models::{
    Actor, Complexity, DecisionInput, GhState, Obligation, ObligationKind, Project, ProjectId,
    Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId,
    TaskState,
};
use tasks::store::Store;

/// Seed a project once; tasks and specs hang off it.
async fn seed_project(store: &Store) -> Project {
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
    };
    store.insert_project(&project).await.unwrap();
    project
}

/// A task with a scouted spec on it, queued at `status`.
async fn seed_spec(
    store: &Store,
    project: &Project,
    issue: u64,
    files: &[&str],
    status: SpecQueueStatus,
) -> Spec {
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
    };
    store.insert_task(&task).await.unwrap();
    seed_spec_for_task(store, &task.id, files, status).await
}

/// Another spec on an existing task — a re-scout.
async fn seed_spec_for_task(
    store: &Store,
    task_id: &TaskId,
    files: &[&str],
    status: SpecQueueStatus,
) -> Spec {
    let now = Utc::now();
    let session = Session {
        id: SessionId::new(),
        task_id: task_id.clone(),
        vm_id: None,
        branch: "scout/x".into(),
        status: SessionStatus::ScoutSucceeded,
        started_at: now,
        completed_at: Some(now),
        exit_reason: None,
        usage: None,
    };
    store.insert_session(&session).await.unwrap();
    let spec = Spec {
        id: SpecId::new(),
        session_id: Some(session.id),
        task_id: task_id.clone(),
        content: "## Spec".into(),
        complexity: Complexity::Simple,
        files_touched: files.iter().map(|f| f.to_string()).collect(),
        created_at: now,
    };
    store.insert_spec(&spec).await.unwrap();
    store
        .upsert_spec_queue_entry(&SpecQueueEntry {
            spec_id: spec.id.clone(),
            status,
            rank: None,
            approved_at: None,
            feedback: None,
            blocking_dependencies: vec![],
        })
        .await
        .unwrap();
    spec
}

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

/// The duplicate-work catch, in the form that does not depend on anyone having
/// been paying attention: two live specs reaching for the same files.
#[tokio::test]
async fn overlapping_live_specs_are_reported_and_dead_ones_are_not() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;

    let subject = seed_spec(
        &store,
        &project,
        810,
        &["src/store.rs", "src/run.rs", "src/only_mine.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;
    let neighbour = seed_spec(
        &store,
        &project,
        811,
        &["src/store.rs", "src/run.rs"],
        SpecQueueStatus::Approved,
    )
    .await;
    // Settled work is not context: a rejected spec's files are nobody's plan.
    seed_spec(
        &store,
        &project,
        812,
        &["src/store.rs"],
        SpecQueueStatus::Rejected,
    )
    .await;

    let brief = Brief::new(&store, None, "main");
    let lines = brief.for_spec(&subject.id).await.unwrap();
    let text = joined(&lines);

    assert!(text.contains("shares 2 files"), "{text}");
    assert!(text.contains(&neighbour.id.to_string()), "{text}");
    assert!(text.contains("#811"), "{text}");
    assert!(text.contains("src/store.rs"), "{text}");
    // The file only this spec touches is not overlap.
    assert!(!text.contains("only_mine.rs"), "{text}");
    assert_eq!(
        text.matches("shares").count(),
        1,
        "the rejected spec should not be reported: {text}"
    );
}

/// The migration collision, in the half that needs no network: two in-flight
/// specs both claiming `0009`. Different filenames, so file overlap misses it
/// entirely — this is the check that catches it.
#[tokio::test]
async fn two_specs_claiming_one_sequence_number_clash() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;

    let subject = seed_spec(
        &store,
        &project,
        810,
        &["crates/tasks/migrations/0009_watermark.sql"],
        SpecQueueStatus::PendingReview,
    )
    .await;
    let other = seed_spec(
        &store,
        &project,
        811,
        &["crates/tasks/migrations/0009_sessions.sql"],
        SpecQueueStatus::Approved,
    )
    .await;

    let text = joined(&brief_lines(&store, &subject.id).await);
    assert!(text.contains("0009"), "{text}");
    assert!(text.contains(&other.id.to_string()), "{text}");
    assert!(text.contains("0009_sessions.sql"), "{text}");
    // No shared path, so this could only have come from the numbering check.
    assert!(!text.contains("shares"), "{text}");
}

/// The memory a restarted session does not have: what was already decided
/// about this task, and why.
#[tokio::test]
async fn a_prior_verdict_on_the_same_task_is_recalled() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;

    let first = seed_spec(
        &store,
        &project,
        810,
        &["src/a.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;
    store
        .review_spec(
            &first.id,
            SpecQueueStatus::Rejected,
            Some("feedback".into()),
            DecisionInput {
                actor: Actor::Orchestrator,
                rationale: Some("rebuilds a harness that already exists\nsecond line".into()),
                evidence: None,
            },
        )
        .await
        .unwrap();

    // The re-scout.
    let second = seed_spec_for_task(
        &store,
        &first.task_id,
        &["src/a.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;

    let text = joined(&brief_lines(&store, &second.id).await);
    assert!(text.contains("an earlier spec for this task"), "{text}");
    assert!(text.contains("reject"), "{text}");
    assert!(text.contains("orchestrator"), "{text}");
    assert!(text.contains("rebuilds a harness"), "{text}");
    // Rationale is summarized to its first line, not pasted whole.
    assert!(!text.contains("second line"), "{text}");
    // Specs for the same task are prior verdicts, never "overlap" — saying
    // both would double-count a re-scout as duplicate work.
    assert!(!text.contains("shares"), "{text}");
}

/// A spec with nothing against it must not produce the same silence as a brief
/// that failed to run — the two mean opposite things to a reviewer deciding
/// how hard to look.
#[tokio::test]
async fn a_clean_brief_says_it_is_clean() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = seed_project(&store).await;
    let spec = seed_spec(
        &store,
        &project,
        810,
        &["src/lonely.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;

    assert!(
        brief_lines(&store, &spec.id).await.is_empty(),
        "nothing to report at the fact level"
    );

    // ...but the turn that carries it says so out loud.
    let brief = Brief::new(&store, None, "main");
    let turn = tasks::orchestrator::format_obligations(
        &store,
        &brief,
        &[Obligation {
            kind: ObligationKind::ReviewSpec,
            subject_id: spec.id.to_string(),
            summary: "#810 has been waiting".into(),
            since: Utc::now(),
        }],
    )
    .await;
    assert!(turn.contains("[brief]"), "{turn}");
    assert!(turn.contains("no file overlap"), "{turn}");
    assert!(turn.contains("#810"), "{turn}");
    // And it is honest about its own scope.
    assert!(turn.contains("not a verdict"), "{turn}");
}

/// Nothing GitHub could have answered means nothing to apologize for. The
/// skipped-check note has to stay rare enough to be worth reading.
#[tokio::test]
async fn the_skipped_github_note_appears_only_when_github_had_something_to_say() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;

    let plain = seed_spec(
        &store,
        &project,
        810,
        &["src/plain.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;
    let numbered = seed_spec(
        &store,
        &project,
        811,
        &["migrations/0012_thing.sql"],
        SpecQueueStatus::PendingReview,
    )
    .await;

    let plain_text = joined(&brief_lines(&store, &plain.id).await);
    assert!(
        !plain_text.contains("GitHub"),
        "no PR overlap and no numbered file: {plain_text}"
    );

    let numbered_text = joined(&brief_lines(&store, &numbered.id).await);
    assert!(
        numbered_text.contains("GitHub was not consulted"),
        "{numbered_text}"
    );
    assert!(numbered_text.contains("no token"), "{numbered_text}");
}

/// A blocked spec's obligation says work stopped; the brief has to say what
/// actually failed, or the decision it asks for is a guess.
#[tokio::test]
async fn a_blocked_spec_brief_names_the_failures() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let spec = seed_spec(
        &store,
        &project,
        810,
        &["src/a.rs"],
        SpecQueueStatus::PendingReview,
    )
    .await;
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            DecisionInput::human(),
        )
        .await
        .unwrap();

    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store
        .finalize_build_failed(&build.id, "agent exited 1")
        .await
        .unwrap();

    let brief = Brief::new(&store, None, "main");
    let text = joined(&brief.for_blocked_spec(&spec.id).await.unwrap());
    assert!(text.contains("agent exited 1"), "{text}");
    assert!(text.contains(&build.id.to_string()), "{text}");
}

/// The two lines of `pipeline` describe two populations, and a reader who
/// cannot tell them apart learns to re-derive both. A spec keeps its
/// `approved` status for the whole build carrying it, so the waiting count has
/// to subtract what the build lines just named.
#[tokio::test]
async fn specs_a_build_is_carrying_are_not_also_counted_as_waiting() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;

    let first = approved_spec(&store, &project, 827, &["src/a.rs"]).await;
    let second = approved_spec(&store, &project, 826, &["src/b.rs"]).await;

    let brief = Brief::new(&store, None, "main");
    let text = joined(&brief.pipeline().await.unwrap());
    assert!(text.contains("2 approved spec(s)"), "{text}");
    assert!(text.contains("in no build yet"), "{text}");

    // Dispatched, but still `approved` in the queue: the build line names it,
    // so the waiting count must not.
    store
        .create_build(
            std::slice::from_ref(&first.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let text = joined(&Brief::new(&store, None, "main").pipeline().await.unwrap());
    assert!(text.contains("#827"), "{text}");
    assert!(text.contains("1 approved spec(s)"), "{text}");
    assert!(text.contains("in no build yet"), "{text}");

    // `queued` counts as carried and so does `running` — builds are serial, so
    // a batch waiting its turn has still already been asked for.
    let claimed = store.claim_next_queued_build().await.unwrap();
    assert!(claimed.is_some(), "the queued build should be claimable");
    let text = joined(&Brief::new(&store, None, "main").pipeline().await.unwrap());
    assert!(text.contains("is running"), "{text}");
    assert!(text.contains("#827"), "{text}");
    assert!(text.contains("1 approved spec(s)"), "{text}");

    // Nothing left over: the line goes away rather than reporting zero.
    store
        .create_build(
            std::slice::from_ref(&second.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let text = joined(&Brief::new(&store, None, "main").pipeline().await.unwrap());
    assert!(text.contains("#826"), "{text}");
    assert!(!text.contains("approved spec(s)"), "{text}");
}

/// An approved spec by the path a real one takes: `create_build` requires a
/// genuine queue status, and the obligations path a genuine `approved_at`, so
/// seeding `Approved` directly would not be the same thing.
async fn approved_spec(store: &Store, project: &Project, issue: u64, files: &[&str]) -> Spec {
    let spec = seed_spec(store, project, issue, files, SpecQueueStatus::PendingReview).await;
    store
        .review_spec(
            &spec.id,
            SpecQueueStatus::Approved,
            None,
            DecisionInput::human(),
        )
        .await
        .unwrap();
    spec
}

async fn brief_lines(store: &Store, spec_id: &SpecId) -> Vec<String> {
    Brief::new(store, None, "main")
        .for_spec(spec_id)
        .await
        .unwrap()
}
