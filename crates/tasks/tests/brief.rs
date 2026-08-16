//! Computed briefs against a real store.
//!
//! These assert the *facts*, not the phrasing beyond the load-bearing words —
//! a brief exists to make a judgment cheap, so what matters is that the thing
//! a reviewer would have had to go and find is present, and that a check which
//! came back clean cannot be mistaken for a check that never ran.
//!
//! Most GitHub-derived facts are absent here on purpose: the client has no
//! token, which is the same degraded path a deployment without `GITHUB_TOKEN`
//! runs, and the brief is required to say so rather than shrink quietly. The
//! landing facts are the exception — those exist to be live, so they get a
//! real axum REST root on loopback and the assertions are on what was *asked*
//! as much as on what came back.

use std::sync::{Arc, Mutex};

use axum::Json as AxumJson;
use axum::extract::{Path as AxumPath, State};
use chrono::Utc;
use serde_json::{Value, json};
use tasks::brief::Brief;
use tasks::github::GitHubClient;
use tasks::models::{
    Actor, Build, Complexity, DecisionInput, GhState, Obligation, ObligationKind, Project,
    ProjectId, Session, SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus,
    Task, TaskId, TaskState,
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

/// A batch parked behind a PR stacked on another build's branch. The brief has
/// to name the base and spell out that merging this PR ships nothing — that
/// misreading is how PR #863 was lost, and a branch name does not carry it.
#[tokio::test]
async fn a_stranded_build_brief_spells_out_a_base_that_is_not_the_trunk() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let spec = approved_spec(&store, &project, 878, &["src/a.rs"]).await;

    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "build/underneath",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    store
        .finalize_build_succeeded(&build.id, "headsha", 863, None, &[])
        .await
        .unwrap();

    let brief = Brief::new(&store, None, "main");
    let text = joined(&brief.for_stranded_build(&build.id).await.unwrap());
    assert!(text.contains("build/underneath"), "{text}");
    assert!(text.contains("NOT the trunk"), "{text}");
    assert!(text.contains("#863"), "{text}");
    assert!(
        text.contains("#878"),
        "names whose issue is waiting: {text}"
    );

    // The obligation's section heading is the *build*, not a spec conjured out
    // of a build id — the failure mode `format_obligations` branches to avoid.
    let turn = tasks::orchestrator::format_obligations(
        &store,
        &brief,
        &[Obligation {
            kind: ObligationKind::LandBatch,
            subject_id: build.id.to_string(),
            summary: "PR #863 has been open".into(),
            since: Utc::now(),
        }],
    )
    .await;
    assert!(turn.contains(&format!("On build {}", build.id)), "{turn}");
    assert!(turn.contains("NOT the trunk"), "{turn}");
}

/// The unstacked case reads differently on purpose: merging the PR really does
/// ship the work, and saying so is what makes the stacked warning worth reading.
#[tokio::test]
async fn a_stranded_build_based_on_the_trunk_says_merging_ships_it() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let spec = approved_spec(&store, &project, 879, &["src/b.rs"]).await;

    let build = store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    store
        .finalize_build_succeeded(&build.id, "headsha", 864, None, &[])
        .await
        .unwrap();

    let brief = Brief::new(&store, None, "main");
    let text = joined(&brief.for_stranded_build(&build.id).await.unwrap());
    assert!(text.contains("its base IS the trunk"), "{text}");
    assert!(!text.contains("NOT the trunk"), "{text}");
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

// --- the landing half: a real GitHub on loopback ---

/// What the fake answers with, and what it was asked.
struct FakeGh {
    pr: Value,
    /// `identical`/`behind` mean the ref is on the trunk; anything else means
    /// it is not.
    compare_status: &'static str,
    /// Every `{base}...{head}` asked about — recording it is what lets a test
    /// assert both that the operands are the right way round and that the
    /// unstacked case costs no compare at all.
    compares: Vec<String>,
}

async fn spawn_fake_github(
    pr: Value,
    compare_status: &'static str,
) -> (String, Arc<Mutex<FakeGh>>) {
    let fake = Arc::new(Mutex::new(FakeGh {
        pr,
        compare_status,
        compares: Vec::new(),
    }));
    let app = axum::Router::new()
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            axum::routing::get(
                move |State(f): State<Arc<Mutex<FakeGh>>>,
                      AxumPath((_o, _r, _n)): AxumPath<(String, String, u64)>| async move {
                    AxumJson(f.lock().unwrap().pr.clone())
                },
            ),
        )
        // A wildcard, not a plain segment: a base branch is `build/<id>`, so
        // the path legitimately carries a slash. With `{basehead}` the fake
        // 404s and the brief truthfully reports "could not be checked", which
        // reads exactly like a logic bug and is not one.
        .route(
            "/repos/{owner}/{repo}/compare/{*basehead}",
            axum::routing::get(
                move |State(f): State<Arc<Mutex<FakeGh>>>,
                      AxumPath((_o, _r, basehead)): AxumPath<(String, String, String)>| async move {
                    let mut f = f.lock().unwrap();
                    f.compares.push(basehead);
                    AxumJson(json!({ "status": f.compare_status }))
                },
            ),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fake)
}

fn open_pr(mergeable_state: &str) -> Value {
    json!({
        "state": "open",
        "merged": false,
        "mergeable": true,
        "mergeable_state": mergeable_state,
        "merge_commit_sha": "speculative",
        "base": { "ref": "main" },
    })
}

/// A succeeded build on `base`, carrying one spec, with `summary` and
/// `files_touched` as the Builder left them.
async fn parked_build(
    store: &Store,
    project: &Project,
    issue: u64,
    base: &str,
    pr_number: u64,
    summary: Option<&str>,
    files: &[&str],
) -> Build {
    let spec = approved_spec(store, project, issue, files).await;
    let build = store
        .create_build(std::slice::from_ref(&spec.id), base, DecisionInput::human())
        .await
        .unwrap();
    store.claim_next_queued_build().await.unwrap().unwrap();
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    store
        .finalize_build_succeeded(&build.id, "headsha", pr_number, summary, &files)
        .await
        .unwrap()
}

/// The ordinary case the whole thing exists for: an unstacked PR GitHub has
/// nothing against, backed by the build's own passing run, touching only code
/// a `make test` reaches. All three facts present — and no compare, because a
/// PR based on the trunk has its answer in `base_ref` already.
#[tokio::test]
async fn a_clean_tested_trunk_based_batch_names_all_three_facts_and_spends_no_compare() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        881,
        "main",
        900,
        Some("Did the thing.\n\nVerification: PASSED — make test (579 tests)"),
        &["crates/tasks/src/brief.rs"],
    )
    .await;
    let (rest, fake) = spawn_fake_github(open_pr("clean"), "ahead").await;
    let github = GitHubClient::new("token").with_rest_base_url(rest);

    let brief = Brief::new(&store, Some(&github), "main");
    let text = joined(&brief.for_stranded_build(&build.id).await.unwrap());

    // 1. What GitHub says about the merge — and what it does not say.
    assert!(text.contains("PR #900 is open"), "{text}");
    assert!(
        text.contains("nothing in the way of the merge itself"),
        "{text}"
    );
    assert!(
        text.contains("says nothing about whether the change works"),
        "a clean verdict must not read as a clearance: {text}"
    );
    // 2. The build's own run, attributed as a claim.
    assert!(text.contains("PASSED"), "{text}");
    assert!(text.contains("make test (579 tests)"), "{text}");
    assert!(text.contains("its claim, not an independent run"), "{text}");
    // 3. What could have checked it.
    assert!(text.contains("make test"), "{text}");
    assert!(!text.contains("Mac"), "nothing here needs one: {text}");

    assert!(
        fake.lock().unwrap().compares.is_empty(),
        "an unstacked PR must cost no compare: {:?}",
        fake.lock().unwrap().compares
    );
}

/// A verdict GitHub would act on has to be reported as one — and both shapes
/// of refusal say what would be refused, not merely that something is wrong.
#[tokio::test]
async fn a_refused_merge_says_which_refusal_it_is() {
    for (state, expected) in [
        (
            "blocked",
            "a required review or status check has not passed",
        ),
        ("dirty", "the branch conflicts with its base"),
    ] {
        let store = Store::open_in_memory().await.unwrap();
        let project = seed_project(&store).await;
        let build = parked_build(
            &store,
            &project,
            882,
            "main",
            901,
            Some("Verification: PASSED — make test"),
            &["crates/tasks/src/run.rs"],
        )
        .await;
        let (rest, _fake) = spawn_fake_github(open_pr(state), "ahead").await;
        let github = GitHubClient::new("token").with_rest_base_url(rest);

        let text = joined(
            &Brief::new(&store, Some(&github), "main")
                .for_stranded_build(&build.id)
                .await
                .unwrap(),
        );
        assert!(text.contains("would refuse this merge"), "{state}: {text}");
        assert!(text.contains(expected), "{state}: {text}");
    }
}

/// The stacked case, in the order the operands have to be in: `trunk...ref`,
/// where reachable reads as `identical`/`behind`. Reversing them inverts the
/// verdict, which is how a batch that shipped nothing gets called done.
#[tokio::test]
async fn a_stacked_pr_reports_its_base_and_asks_the_compare_in_that_order() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        883,
        "build/underneath",
        902,
        Some("Verification: PASSED — make test"),
        &["crates/tasks/src/store.rs"],
    )
    .await;
    // The base has not landed: `main...build/underneath` is `ahead`.
    let (rest, fake) = spawn_fake_github(open_pr("clean"), "ahead").await;
    let github = GitHubClient::new("token").with_rest_base_url(rest);

    let text = joined(
        &Brief::new(&store, Some(&github), "main")
            .for_stranded_build(&build.id)
            .await
            .unwrap(),
    );
    assert!(text.contains("has not reached main yet"), "{text}");
    assert!(text.contains("ships nothing until"), "{text}");
    assert_eq!(
        fake.lock().unwrap().compares,
        vec!["main...build/underneath".to_string()],
        "trunk first, ref second — reversed, the verdict inverts"
    );
}

/// The other stack order, which is not merely the negation: a base that has
/// *already* landed means merging this PR adds a commit to a branch nothing
/// will pick up, and the fix is to retarget rather than to wait.
#[tokio::test]
async fn a_stacked_pr_whose_base_already_landed_wants_retargeting() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        884,
        "build/underneath",
        903,
        Some("Verification: PASSED — make test"),
        &["crates/tasks/src/store.rs"],
    )
    .await;
    let (rest, _fake) = spawn_fake_github(open_pr("clean"), "behind").await;
    let github = GitHubClient::new("token").with_rest_base_url(rest);

    let text = joined(
        &Brief::new(&store, Some(&github), "main")
            .for_stranded_build(&build.id)
            .await
            .unwrap(),
    );
    assert!(text.contains("ALREADY reached main"), "{text}");
    assert!(text.contains("retargeting"), "{text}");
}

/// The case every batch parked today falls into, and the one a mistake has to
/// fall towards: GitHub is content, and there is still no evidence the change
/// works.
#[tokio::test]
async fn a_batch_with_no_verification_line_says_so_next_to_the_clean_verdict() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        885,
        "main",
        904,
        Some("Implemented the spec. Refactored two helpers."),
        &["crates/tasks/src/brief.rs"],
    )
    .await;
    let (rest, _fake) = spawn_fake_github(open_pr("clean"), "ahead").await;
    let github = GitHubClient::new("token").with_rest_base_url(rest);

    let text = joined(
        &Brief::new(&store, Some(&github), "main")
            .for_stranded_build(&build.id)
            .await
            .unwrap(),
    );
    assert!(
        text.contains("nothing in the way of the merge itself"),
        "{text}"
    );
    assert!(text.contains("no test run at all"), "{text}");
    assert!(text.contains("unknown rather than known-skipped"), "{text}");
}

/// The narrow carve-out, stated narrowly: the app compiles and unit-tests on a
/// Linux builder, and only the rendering does not.
#[tokio::test]
async fn an_app_only_batch_says_a_mac_is_needed_and_a_tokenless_server_says_so() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        886,
        "main",
        905,
        Some("Verification: PASSED — make app-test"),
        &["app-gpui/src/sections/queue.rs", "app-gpui/src/lib.rs"],
    )
    .await;

    let text = joined(
        &Brief::new(&store, None, "main")
            .for_stranded_build(&build.id)
            .await
            .unwrap(),
    );
    assert!(text.contains("Mac"), "{text}");
    assert!(text.contains("make app-test"), "{text}");
    // No token: mergeability is unchecked, and a brief must never let that
    // read as clean.
    assert!(text.contains("GitHub was not consulted"), "{text}");
    assert!(text.contains("unchecked rather than fine"), "{text}");
}

/// What is parked has to reach the pipeline section, or an open PR is invisible
/// in exactly the place a reader looks for what is in flight. Sourced from the
/// store rather than `world.builds`, which drops anything older than 14 days —
/// the batch stranded longest.
#[tokio::test]
async fn the_pipeline_names_a_parked_batch_its_pr_and_its_issue() {
    let store = Store::open_in_memory().await.unwrap();
    let project = seed_project(&store).await;
    let build = parked_build(
        &store,
        &project,
        887,
        "main",
        906,
        Some("Verification: PASSED — make test"),
        &["crates/tasks/src/run.rs"],
    )
    .await;

    let text = joined(&Brief::new(&store, None, "main").pipeline().await.unwrap());
    assert!(text.contains(&build.id.to_string()), "{text}");
    assert!(text.contains("PR #906"), "{text}");
    assert!(text.contains("#887"), "{text}");
    assert!(text.contains("awaiting merge"), "{text}");
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
