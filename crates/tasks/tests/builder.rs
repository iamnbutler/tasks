//! End-to-end Builder dispatch integration test.
//!
//! Spins up:
//! - A real vm-pool-service pointing at the real builder-supervisor binary.
//! - A real vm-pool-client over a Unix socket.
//! - A real SQLite store.
//! - A real local git repo as the "remote" (non-bare — pushing works for any
//!   branch that isn't the checked-out one, which is what makes a no-mock
//!   push test possible at all).
//! - A real HTTP server standing in for GitHub's REST API, recording the PR
//!   request it receives.
//!
//! Then runs `Builder::dispatch(build, clone_url)` and asserts the branch
//! actually lands in the remote and the store drains the batch. No mocks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::Json as AxumJson;
use axum::routing::post;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::process::Command;

use tasks::builder::{Builder, BuilderConfig};
use tasks::github::GitHubClient;
use tasks::models::{
    BuildStatus, Complexity, DecisionInput, GhState, Project, ProjectId, ProjectStatus, Session,
    SessionId, SessionStatus, Spec, SpecId, SpecQueueEntry, SpecQueueStatus, Task, TaskId,
    TaskState, TranscriptOwner, TranscriptStream,
};
use tasks::store::Store;
use tasks_protocol::TasksProtocol;
use vm_pool_client::Client;
use vm_pool_protocol::VmConfig;

mod common;
use common::{
    api_death_builder_agent_path, echo_prompt_builder_agent_path, gated_builder_agent_path,
    history_rewriting_builder_agent_path, make_fixture_repo, silent_builder_agent_path,
    spawn_vm_pool, stub_builder_agent_path, wait_until, workspace_bin,
    write_builder_supervisor_wrapper_with_env,
};

async fn run_git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Fake GitHub REST endpoint: answers POST /repos/{owner}/{repo}/pulls with
/// a fixed PR number and records the request body.
async fn spawn_fake_github_rest(number: u64) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let state = seen.clone();
    let app =
        axum::Router::new()
            .route(
                "/repos/{owner}/{repo}/pulls",
                post(
                    move |State(s): State<Arc<Mutex<Vec<Value>>>>,
                          AxumJson(body): AxumJson<Value>| async move {
                        s.lock().unwrap().push(body);
                        AxumJson(json!({ "number": number }))
                    },
                ),
            )
            .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// Seed a project plus the full chain a build consumes: a ready_to_build
/// task, its scout session, its spec, and an approved queue entry.
async fn seed_approved(store: &Store, project: &Project, issue: u64, title: &str) -> (Task, Spec) {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: title.into(),
        body: "issue body".into(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::ReadyToBuild,
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
        branch: format!("scout/{}", task.id),
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
        task_id: task.id.clone(),
        content: format!("## Spec: {title}\n\nAdd a function for issue {issue}."),
        complexity: Complexity::Simple,
        files_touched: vec![],
        created_at: now,
    };
    store.insert_spec(&spec).await.unwrap();
    store
        .upsert_spec_queue_entry(&SpecQueueEntry {
            spec_id: spec.id.clone(),
            status: SpecQueueStatus::Approved,
            rank: None,
            approved_at: Some(now),
            feedback: None,
            blocking_dependencies: vec![],
        })
        .await
        .unwrap();
    (task, spec)
}

struct Harness {
    store: Arc<Store>,
    builder: Builder,
    project: Project,
    repo_url: String,
    repo: std::path::PathBuf,
    /// Where per-build scratch repos live — and, under `rejected/`, the
    /// bundles egress could not push.
    scratch_root: std::path::PathBuf,
    seen_prs: Arc<Mutex<Vec<Value>>>,
    /// The real vm-pool socket. A test that wants to stand something between
    /// the client and the service connects its own listener to this.
    pool_socket: std::path::PathBuf,
    github: Arc<GitHubClient>,
    tmp: tempfile::TempDir,
}

async fn harness(agent_cmd: &str) -> Harness {
    harness_with_env(agent_cmd, &[]).await
}

/// [`harness`], plus environment the builder-supervisor reads *inside* the VM
/// (`BUILDER_MAX_RESUMES` has no other seam from out here).
async fn harness_with_env(agent_cmd: &str, supervisor_env: &[(&str, &str)]) -> Harness {
    harness_with_env_and_vms(agent_cmd, supervisor_env, 1).await
}

/// [`harness_with_env`] with room for more than one VM. A test that cuts the
/// connection needs it: a `deallocate` cannot travel down a socket that is
/// gone, so every such round strands a VM in the pool (a real vm-pool restart
/// collects these on the next connect; nothing in a test can).
async fn harness_with_env_and_vms(
    agent_cmd: &str,
    supervisor_env: &[(&str, &str)],
    max_vms: usize,
) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let supervisor_bin = workspace_bin("builder-supervisor").await;
    let wrapper = write_builder_supervisor_wrapper_with_env(
        tmp.path(),
        &supervisor_bin,
        agent_cmd,
        tmp.path(),
        supervisor_env,
    )
    .await;
    let (_service, socket) = spawn_vm_pool(tmp.path(), &wrapper, max_vms).await;
    let client = Client::<TasksProtocol>::connect(&socket).await.unwrap();

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();

    let repo = make_fixture_repo(tmp.path(), "remote-repo").await;
    let repo_url = format!("file://{}", repo.display());

    let (rest_url, seen_prs) = spawn_fake_github_rest(42).await;
    let github = Arc::new(
        GitHubClient::with_base_url("token", "http://unused.invalid/graphql")
            .with_rest_base_url(rest_url),
    );
    let github_for_harness = github.clone();

    let scratch_root = tmp.path().join("scratch");
    let builder = Builder::new(
        store.clone(),
        client.handle(),
        github,
        BuilderConfig {
            image: "builder:test".into(),
            vm_config: VmConfig::default(),
            timeout: Duration::from_secs(60),
            leases: None,
            trunk_branch: "main".into(),
            scratch_root: scratch_root.clone(),
        },
    );

    Harness {
        store,
        builder,
        project,
        repo_url,
        repo,
        scratch_root,
        seen_prs,
        pool_socket: socket,
        github: github_for_harness,
        tmp,
    }
}

#[tokio::test]
async fn a_batch_of_two_specs_lands_as_one_branch_and_one_pr() {
    let h = harness(stub_builder_agent_path().to_str().unwrap()).await;
    let (task_a, spec_a) = seed_approved(&h.store, &h.project, 7, "First thing").await;
    let (task_b, spec_b) = seed_approved(&h.store, &h.project, 9, "Second thing").await;

    let build = h
        .store
        .create_build(
            &[spec_a.id.clone(), spec_b.id.clone()],
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
    assert_eq!(claimed.id, build.id);

    let done = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap();
    assert_eq!(done.status, BuildStatus::Succeeded);
    assert_eq!(done.pr_number, Some(42));
    let head_sha = done.head_sha.clone().expect("head sha recorded");
    assert!(done.base_sha.is_some(), "base sha recorded from Started");
    assert!(
        done.files_touched.contains(&"src/built.rs".to_string()),
        "files: {:?}",
        done.files_touched
    );
    // Two clocks, in order: the agent phase ends when the drain does, and
    // `completed_at` waits for teardown, the push, and the PR.
    let agent_finished = done
        .agent_finished_at
        .expect("the agent phase was stamped when the drain ended");
    assert!(done.started_at.unwrap() <= agent_finished);
    assert!(agent_finished <= done.completed_at.unwrap());

    // The image identity, all the way through the real supervisor binary. See
    // the Scout counterpart in `tests/scout.rs`: these two tests are the only
    // place the chain is actually checked, because `Option` plus
    // `serde(default)` makes a break indistinguishable from "not sent".
    let images = h.store.image_builds("0.1.0").await.unwrap();
    let observed = images
        .iter()
        .find(|i| i.image == "builder:test")
        .expect("the builder image was observed");
    assert_eq!(observed.role, tasks_api::version::ImageRole::Builder);
    assert!(
        observed.version.is_some(),
        "the supervisor stated no build identity"
    );
    assert!(observed.commit.is_some());
    assert_eq!(observed.run_id.as_deref(), Some(done.id.as_str()));

    // The branch REALLY landed: the remote repo has it, at the reported tip.
    let branch_ref = format!("refs/heads/{}", done.branch);
    let tip = run_git(&h.repo, &["rev-parse", &branch_ref]).await;
    assert_eq!(tip, head_sha);
    let listing = run_git(&h.repo, &["ls-tree", "-r", "--name-only", &tip]).await;
    assert!(listing.contains("src/built.rs"));
    assert!(!listing.contains("PROMPT.md") && !listing.contains("SUMMARY.md"));

    // The PR request the fake GitHub saw. Block-scoped (not `drop()`) so the
    // guard is provably not held across the awaits below.
    {
        let prs = h.seen_prs.lock().unwrap();
        assert_eq!(prs.len(), 1);
        let pr = &prs[0];
        assert_eq!(pr["head"], json!(done.branch));
        assert_eq!(pr["base"], json!("main"));
        assert_eq!(pr["title"], json!("Build: #7, #9"));
        let body = pr["body"].as_str().unwrap();
        assert!(body.contains("Implements #7") && body.contains("Implements #9"));
        assert!(
            !body.contains("Closes"),
            "closing an issue is not ours to write"
        );
    }

    // A successful build is transcribed too — the endpoint is not a
    // failure-only diagnostic.
    let lines = h
        .store
        .transcript_since(&TranscriptOwner::build(&done.id), 0, 1000)
        .await
        .unwrap();
    assert!(!lines.is_empty(), "a successful build recorded nothing");
    assert!(
        lines
            .iter()
            .any(|l| l.line.contains("[stub-builder] starting")),
        "the agent's own output is missing: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.line == "[tasks] builder agent exited with code 0"),
        "the exit-code line is missing from a successful build"
    );

    // The batch drained: specs built, tasks done.
    for (task, spec) in [(&task_a, &spec_a), (&task_b, &spec_b)] {
        assert_eq!(
            h.store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Built
        );
        // Not `done`: a PR that opened is a claim, not a delivery. The
        // batch parks here until the poller reads the pull request.
        assert_eq!(
            h.store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::AwaitingMerge
        );
    }
}

#[tokio::test]
async fn a_failed_build_returns_the_work_without_wedging_the_queue() {
    // `true` as the agent: exits 0 having committed nothing -> empty branch.
    let h = harness("true").await;
    let (task, spec) = seed_approved(&h.store, &h.project, 7, "First thing").await;

    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();

    let err = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no commits"), "{err}");
    // An agent that ran to completion and committed nothing judged the specs,
    // and burns one of their three. Waiving this would be switching the cap
    // off — see `a_transport_death_costs_the_batch_no_build_attempt` for the
    // case that is waived.
    assert_eq!(
        err.failure_class(),
        tasks::protocol::FailureClass::Verdict,
        "{err:?}"
    );

    let after = h.store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(after.status, BuildStatus::Failed);
    assert!(
        after
            .exit_reason
            .as_deref()
            .unwrap_or("")
            .contains("no commits"),
        "exit_reason: {:?}",
        after.exit_reason
    );
    // The failure path is the one that hung for 84 minutes inside teardown,
    // charging all of it to the agent. It gets its own clock too.
    let agent_finished = after
        .agent_finished_at
        .expect("a failed drain still ends the agent phase");
    assert!(after.started_at.unwrap() <= agent_finished);
    assert!(agent_finished <= after.completed_at.unwrap());
    // Spec stays approved, task returns to ready_to_build — never further
    // back — and the queue is claimable again.
    assert_eq!(
        h.store
            .get_spec_queue_entry(&spec.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SpecQueueStatus::Approved
    );
    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
    assert!(
        h.seen_prs.lock().unwrap().is_empty(),
        "no PR for a failed build"
    );

    let again = h
        .store
        .create_build(&[spec.id], "main", DecisionInput::human())
        .await
        .unwrap();
    assert_eq!(
        h.store.claim_next_queued_build().await.unwrap().unwrap().id,
        again.id
    );
}

/// #884 for Diamond 2: a build whose agent lost its API connection commits
/// nothing and fails — but it never judged the specs, so it spends none of
/// their three attempts.
///
/// Four consecutive deaths, one past the cap. Before this the batch would have
/// been `blocked` after the third, having learned nothing about whether the
/// work can be built.
#[tokio::test]
async fn a_transport_death_costs_the_batch_no_build_attempt() {
    // Resuming off: the supervisor's own retry loop is #845's and is tested
    // there. What is under test here is the host's accounting.
    let h = harness_with_env(
        api_death_builder_agent_path().to_str().unwrap(),
        &[("BUILDER_MAX_RESUMES", "0")],
    )
    .await;
    let (task, spec) = seed_approved(&h.store, &h.project, 7, "The network, not the spec").await;

    for attempt in 1..=4 {
        h.store
            .create_build(
                std::slice::from_ref(&spec.id),
                "main",
                DecisionInput::human(),
            )
            .await
            .unwrap_or_else(|e| panic!("attempt {attempt} could not be queued: {e}"));
        let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
        let err = h
            .builder
            .dispatch(
                claimed,
                &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
            )
            .await
            .unwrap_err();

        // The symptom is still the symptom — the class is what the host reads.
        assert!(format!("{err}").contains("no commits"), "{err}");
        assert_eq!(
            err.failure_class(),
            tasks::protocol::FailureClass::Transport,
            "attempt {attempt}: {err:?}"
        );
        assert_eq!(
            h.store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved,
            "attempt {attempt}: the spec is still buildable"
        );
    }

    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
    // An unspent strike has to be legible, or it is indistinguishable from a
    // cap that has been switched off.
    let notes: Vec<String> = h
        .store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            tasks::events::EventPayload::Note { source, message } if source == "dispatcher" => {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert!(
        notes
            .iter()
            .any(|m| m.contains("failed as transport") && m.contains("keep their build attempts")),
        "the waiver has to be on the log: {notes:?}"
    );
}

/// The issue (#825) in one test: a build burns its run, commits nothing, and
/// fails. Before this change all it left behind was `exit_reason: "no
/// commits"` and two timestamps — nothing that said *why*. Now the agent's
/// own account of it is readable through the store, complete by the time the
/// build row is final.
#[tokio::test]
async fn a_silent_build_failure_leaves_a_readable_transcript() {
    let h = harness(silent_builder_agent_path().to_str().unwrap()).await;
    let (_task, spec) = seed_approved(&h.store, &h.project, 7, "Doomed thing").await;

    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();

    let err = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no commits"), "{err}");

    // The build row is final and says only that it failed …
    let after = h.store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(after.status, BuildStatus::Failed);

    // … and the transcript, flushed before that row was finalized, says why.
    let owner = TranscriptOwner::build(&build.id);
    let lines = h.store.transcript_since(&owner, 0, 1000).await.unwrap();
    assert!(
        !lines.is_empty(),
        "the failure left no transcript — this is exactly the bug"
    );
    assert!(lines.iter().all(|l| l.owner == owner));

    // Dense and 1-based, which is what `?since=` paging relies on.
    assert_eq!(
        lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
        (1..=lines.len() as i64).collect::<Vec<_>>()
    );

    // Both pipes, kept apart: the agent's reason went to stderr, its
    // stream-json to stdout.
    let on = |stream| {
        lines
            .iter()
            .filter(move |l| l.stream == stream)
            .map(|l| l.line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stdout = on(TranscriptStream::Stdout);
    let stderr = on(TranscriptStream::Stderr);
    assert!(
        stdout.contains(r#""type":"assistant""#),
        "stream-json missing from stdout:\n{stdout}"
    );
    assert!(
        stderr.contains("giving up without committing"),
        "the agent's stated reason is missing from stderr:\n{stderr}"
    );

    // The server's own line puts the exit status in the same ordered stream.
    assert!(
        lines
            .iter()
            .any(|l| l.line == "[tasks] builder agent exited with code 3"),
        "no exit-code line: {lines:?}"
    );

    // Nothing leaked onto the spec's scout session — seqs restart per owner,
    // and a build's lines belong to the build.
    assert!(
        h.store
            .transcript_since(
                &TranscriptOwner::session(spec.session_id.as_ref().unwrap()),
                0,
                1000
            )
            .await
            .unwrap()
            .is_empty(),
        "build output was written onto the scout session"
    );
}

/// #891 end to end: an agent that tidies its history from a detached HEAD.
///
/// From the detach onwards `refs/heads/<branch>` stops tracking the work, so
/// the supervisor reported one commit and bundled another and the server threw
/// a finished implementation away. What has to hold now is the whole chain: the
/// branch really reaches the remote, at the recorded head, carrying the
/// rewritten history — and the tip the reconciliation decided against rides the
/// bundle without ever being pushed.
#[tokio::test]
async fn a_history_rewriting_build_lands_its_branch() {
    let h = harness(history_rewriting_builder_agent_path().to_str().unwrap()).await;
    let (task, spec) = seed_approved(&h.store, &h.project, 891, "Tidy the series").await;

    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();

    let done = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap();
    assert_eq!(done.status, BuildStatus::Succeeded);
    let head_sha = done.head_sha.clone().expect("head sha recorded");

    // The branch landed in the remote at the recorded head, carrying the
    // rewritten history rather than the ref the agent left behind.
    let branch_ref = format!("refs/heads/{}", done.branch);
    assert_eq!(
        run_git(&h.repo, &["rev-parse", &branch_ref]).await,
        head_sha
    );
    let listing = run_git(&h.repo, &["ls-tree", "-r", "--name-only", &head_sha]).await;
    for file in ["src/one.rs", "src/two.rs", "src/three.rs"] {
        assert!(listing.contains(file), "{file} missing:\n{listing}");
    }
    assert!(!listing.contains("PROMPT.md") && !listing.contains("SUMMARY.md"));

    // The abandoned tip is insurance inside the bundle, never a ref anyone
    // publishes: the server pushes one refspec, by name.
    assert!(
        run_git(&h.repo, &["for-each-ref", "refs/abandoned/"])
            .await
            .is_empty(),
        "refs/abandoned/ reached the remote"
    );

    // And a reviewer reads what happened in the transcript rather than
    // re-deriving it from SHAs in an error string.
    let lines = h
        .store
        .transcript_since(&TranscriptOwner::build(&done.id), 0, 1000)
        .await
        .unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.line.contains("reconciling the build branch")),
        "the reconciliation is not in the transcript: {lines:?}"
    );

    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::AwaitingMerge
    );
    assert_eq!(h.seen_prs.lock().unwrap().len(), 1);
    assert_eq!(
        h.store.get_build(&build.id).await.unwrap().unwrap().status,
        BuildStatus::Succeeded
    );
}

/// #935, end to end: the feedback a spec was **approved with** reaches the
/// agent inside the VM.
///
/// The unit tests in `builder.rs` pin what `render_prompt` writes; only this
/// one pins that the dispatcher goes and *reads that feedback out of the
/// store*, which is the half that was missing — the prompt was assembled from
/// `specs` and `tasks`, and the column lives on `spec_queue`.
///
/// The seam is `SUMMARY.md`: the fixture agent echoes its whole prompt into
/// it, and the summary comes back on the build row, so what the agent was told
/// is assertable from out here without entering the VM.
#[tokio::test]
async fn the_review_feedback_a_spec_was_approved_with_reaches_the_builder_agent() {
    let h = harness(echo_prompt_builder_agent_path().to_str().unwrap()).await;
    let (_, reviewed) = seed_approved(&h.store, &h.project, 935, "Carry the feedback").await;
    let (_, silent) = seed_approved(&h.store, &h.project, 936, "Approved saying nothing").await;

    // The real reviewer path, both channels at once: `feedback` is addressed
    // to the agent, `rationale` to the ledger.
    h.store
        .review_spec(
            &reviewed.id,
            SpecQueueStatus::Approved,
            Some(
                "Name the constant REVIEW_FEEDBACK_HEADING, and say why in the module docs.".into(),
            ),
            DecisionInput {
                actor: tasks::models::Actor::Human,
                rationale: Some(
                    "approving with one required change rather than sending it back".into(),
                ),
                evidence: None,
            },
        )
        .await
        .unwrap();

    h.store
        .create_build(
            &[reviewed.id.clone(), silent.id.clone()],
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
    let done = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap();
    assert_eq!(done.status, BuildStatus::Succeeded);

    let prompt = done.summary.clone().expect("the agent echoed its prompt");
    assert!(
        prompt.contains("## Review feedback on these specs"),
        "the section never reached the VM:\n{prompt}"
    );
    assert!(
        prompt.contains("Name the constant REVIEW_FEEDBACK_HEADING"),
        "the feedback text never reached the VM:\n{prompt}"
    );
    // Attributed to the spec it was written about, and to that one only.
    assert!(
        prompt.contains("### On spec 1 of 2: Carry the feedback (#935)"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("### On spec 2 of 2"),
        "a spec approved with nothing grew a subsection:\n{prompt}"
    );
    // And the accounting it demands is the one the brief reads back.
    assert!(prompt.contains("## Review feedback` heading"), "{prompt}");

    // And the other half of the same rule: `review_spec` took a rationale too,
    // and a decision record addressed to the ledger reaches no VM ever.
    assert!(
        !prompt.contains("rather than sending it back"),
        "the rationale reached a VM:\n{prompt}"
    );
}

/// The VM is deallocated before egress runs, so a bundle the server refuses to
/// push is the only copy of the implementation there is. The assertion is not
/// the error text but the recovery: **the command the server printed is run**,
/// against a fresh clone, and the implementation comes back.
///
/// The remote is pre-loaded with the build's branch on other history, which
/// makes the push a plain non-fast-forward rejection — no mocks, and the
/// failure lands exactly where a real one does.
#[tokio::test]
async fn a_rejected_egress_preserves_the_commits_and_the_command_recovers_them() {
    let h = harness(stub_builder_agent_path().to_str().unwrap()).await;
    let (task, spec) = seed_approved(&h.store, &h.project, 7, "First thing").await;

    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();

    // Somebody else's commit, sitting on the branch name this build will push.
    run_git(&h.repo, &["checkout", "-q", "-b", &build.branch]).await;
    tokio::fs::write(h.repo.join("unrelated.txt"), "someone else was here\n")
        .await
        .unwrap();
    run_git(&h.repo, &["add", "-A"]).await;
    run_git(&h.repo, &["commit", "-q", "-m", "Unrelated work"]).await;
    run_git(&h.repo, &["checkout", "-q", "main"]).await;

    let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
    let err = h
        .builder
        .dispatch(
            claimed,
            &tasks::broker::CloneSource::Direct(h.repo_url.clone()),
        )
        .await
        .unwrap_err();
    let reason = format!("{err}");
    assert!(reason.starts_with("branch egress: "), "{reason}");
    assert!(
        !reason.contains("branch egress: branch egress:"),
        "the prefix was wrapped twice: {reason}"
    );
    assert!(reason.contains("recover them with git fetch"), "{reason}");
    assert!(h.seen_prs.lock().unwrap().is_empty(), "no PR for a failure");

    // The per-build scratch repo is swept; the bundle beside it is not.
    let bundle = h
        .scratch_root
        .join("rejected")
        .join(format!("{}.bundle", build.id));
    assert!(
        bundle.exists(),
        "no preserved bundle at {}",
        bundle.display()
    );
    assert!(
        reason.contains(&bundle.display().to_string()),
        "the reason does not name the bundle: {reason}"
    );
    assert!(
        !h.scratch_root
            .join(format!("scratch-{}", build.id))
            .exists(),
        "the scratch repo outlived the build"
    );

    // Announced, so the file is in front of somebody without anyone going
    // looking: this is the one moment at which the server knows an
    // implementation now exists in exactly one place.
    let preserved = h
        .store
        .events_since(0, 500)
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.payload {
            tasks::events::EventPayload::BundlePreserved { build_id, bytes } => {
                Some((build_id, bytes))
            }
            _ => None,
        })
        .expect("the preservation was announced");
    assert_eq!(preserved.0, build.id);
    assert_eq!(
        preserved.1,
        tokio::fs::metadata(&bundle).await.unwrap().len(),
        "the announced size is the file's"
    );

    // The recovery, run — and run as *printed*: the failure reason is parsed
    // for the command rather than the command being rebuilt here, because a
    // command that only works when the test reconstructs it is not a recovery.
    let tmp = tempfile::tempdir().unwrap();
    let recovered = tmp.path().join("recovered.git");
    tokio::fs::create_dir_all(&recovered).await.unwrap();
    run_git(&recovered, &["init", "--bare"]).await;
    run_git(&recovered, &["fetch", &h.repo_url, "main:refs/heads/main"]).await;
    let printed = reason
        .split_once("recover them with ")
        .expect("the reason names the recovery")
        .1;
    let status = Command::new("sh")
        .arg("-c")
        .arg(printed)
        .current_dir(&recovered)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "the printed recovery failed: {printed}");
    let listing = run_git(&recovered, &["ls-tree", "-r", "--name-only", &build.branch]).await;
    assert!(listing.contains("src/built.rs"), "{listing}");
    assert!(listing.contains("src/forgotten.rs"), "{listing}");

    // And the build is charged for the failure like any other, without
    // wedging the queue.
    let after = h.store.get_build(&build.id).await.unwrap().unwrap();
    assert_eq!(after.status, BuildStatus::Failed);
    assert!(
        after
            .exit_reason
            .as_deref()
            .unwrap_or("")
            .contains("recover them with git fetch"),
        "exit_reason: {:?}",
        after.exit_reason
    );
    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::ReadyToBuild
    );
}

/// A Unix-socket relay the test owns, standing between the Builder's vm-pool
/// client and the real service.
///
/// vm-pool's `Service` has no shutdown API and the test service runs
/// in-process, so "kill the daemon" is not directly expressible — hence this.
/// Cutting it makes the client see genuine EOF, so `StreamClosed` is raised by
/// the code under test rather than constructed by the test. Killing the *VM*
/// instead would be a different failure: what a vm-pool restart does to this
/// server is end the connection, and that is the only thing that raises
/// `StreamClosed`.
struct PoolLink {
    path: std::path::PathBuf,
    pumps: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl PoolLink {
    async fn open(path: std::path::PathBuf, upstream: std::path::PathBuf) -> Self {
        use tokio::net::{UnixListener, UnixStream};

        let listener = UnixListener::bind(&path).unwrap();
        let pumps: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let registry = pumps.clone();
        let accept = tokio::spawn(async move {
            while let Ok((down, _)) = listener.accept().await {
                let Ok(up) = UnixStream::connect(&upstream).await else {
                    return;
                };
                let (mut down_read, mut down_write) = down.into_split();
                let (mut up_read, mut up_write) = up.into_split();
                let to_pool = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut down_read, &mut up_write).await;
                });
                let to_client = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut up_read, &mut down_write).await;
                });
                registry.lock().unwrap().extend([to_pool, to_client]);
            }
        });
        pumps.lock().unwrap().push(accept);
        Self { path, pumps }
    }

    /// Drop both halves of every pumped connection, and the accept loop with
    /// them. Aborting the tasks drops the sockets they own, which is what puts
    /// the EOF on the client's side.
    fn cut(&self) {
        for pump in self.pumps.lock().unwrap().drain(..) {
            pump.abort();
        }
    }
}

impl Drop for PoolLink {
    fn drop(&mut self) {
        self.cut();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// vm-pool is a separate long-lived daemon, upgraded separately and — per
/// CLAUDE.md — restarted *ahead* of this server, so a restart routinely
/// catches a build in flight. `BuilderError::StreamClosed` was classified
/// `Verdict`, four lines below the comment explaining why `Egress` is
/// `Transport`, and `Builder::conclude` reads `failure_class()` straight into
/// `Strike::for_class` — so every such restart charged the whole batch, and
/// three of them `blocked` specs that had never failed to build.
///
/// Four rounds, one past `MAX_BUILD_ATTEMPTS`, so the assertion is "the cap
/// was never reached" rather than "the cap has not been reached yet".
#[tokio::test]
async fn a_vm_pool_that_goes_away_mid_build_costs_the_batch_no_attempt() {
    // The gate is released at the end so nothing is left sleeping; while it is
    // held, the agent is provably still running, which is what puts a live
    // build behind the cut.
    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("build-gate");
    let agent_cmd = format!(
        "{} {}",
        gated_builder_agent_path().display(),
        gate.display()
    );
    // Every round strands a VM: a `deallocate` cannot travel down a socket
    // that is gone.
    let h = harness_with_env_and_vms(&agent_cmd, &[], 8).await;
    let (task, spec) = seed_approved(&h.store, &h.project, 7, "The daemon, not the spec").await;

    let started = gate_dir.path().join("build-gate.started");
    for round in 1..=4u32 {
        // The previous round's agent is stranded but still holding its
        // marker; without clearing it the wait below returns instantly and
        // the cut lands on a build that has not allocated yet.
        let _ = std::fs::remove_file(&started);

        let link = PoolLink::open(
            h.tmp.path().join(format!("relay-{round}.sock")),
            h.pool_socket.clone(),
        )
        .await;
        let client = Client::<TasksProtocol>::connect(&link.path).await.unwrap();
        let builder = Builder::new(
            h.store.clone(),
            client.handle(),
            h.github.clone(),
            BuilderConfig {
                image: "builder:test".into(),
                vm_config: VmConfig::default(),
                timeout: Duration::from_secs(60),
                leases: None,
                trunk_branch: "main".into(),
                scratch_root: h.scratch_root.clone(),
            },
        );

        h.store
            .create_build(
                std::slice::from_ref(&spec.id),
                "main",
                DecisionInput::human(),
            )
            .await
            .unwrap_or_else(|e| panic!("round {round} could not be queued: {e}"));
        let claimed = h.store.claim_next_queued_build().await.unwrap().unwrap();
        let build_id = claimed.id.clone();

        let repo_url = h.repo_url.clone();
        let dispatch = tokio::spawn(async move {
            builder
                .dispatch(claimed, &tasks::broker::CloneSource::Direct(repo_url))
                .await
        });

        // Wait until the agent is provably running, then cut the connection
        // out from under a live build.
        wait_until(Duration::from_secs(30), || {
            let started = started.clone();
            async move { started.exists() }
        })
        .await;
        link.cut();

        let err = dispatch.await.unwrap().unwrap_err();
        assert!(
            matches!(err, tasks::builder::BuilderError::StreamClosed),
            "round {round} raised something else: {err:?}"
        );

        // The build got as far as owning a VM, so the store's own
        // never-started waiver (`finalize_build_unsuccessfully` charges only
        // `if started`) is not what is passing this test.
        let build = h.store.get_build(&build_id).await.unwrap().unwrap();
        assert!(
            build.vm_id.is_some(),
            "round {round}: the build never started, so nothing was waived"
        );

        assert_eq!(
            h.store
                .get_spec_queue_entry(&spec.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SpecQueueStatus::Approved,
            "round {round}: the spec must stay buildable"
        );
        assert_eq!(
            h.store.get_task(&task.id).await.unwrap().unwrap().state,
            TaskState::ReadyToBuild,
            "round {round}"
        );
        // Cleaned up here rather than at end of scope, so the next round's
        // socket path is the only one anything can reach.
        drop(link);
    }

    // `SpecQueueEntry` does not expose `build_attempts`, so the counter is
    // read the way a human would — off the obligation the spec is still
    // raising.
    let obligations = h
        .store
        .open_obligations(chrono::Duration::seconds(-1))
        .await
        .unwrap();
    let dispatch = obligations
        .iter()
        .find(|o| o.subject_id == spec.id.to_string())
        .unwrap_or_else(|| panic!("the spec should still be owed a build: {obligations:?}"));
    assert!(
        dispatch.summary.contains("4 earlier build(s) failed"),
        "{}",
        dispatch.summary
    );
    assert!(
        dispatch.summary.contains("0 of 3 attempts are charged"),
        "four transport deaths must charge nothing: {}",
        dispatch.summary
    );

    // And the waivers are legible on the log, or an unspent attempt is
    // indistinguishable from a cap somebody switched off.
    let waivers = h
        .store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| match &e.payload {
            tasks::events::EventPayload::Note { source, message } => {
                source == "dispatcher"
                    && message.contains("failed as transport")
                    && message.contains("keep their build attempts")
            }
            _ => false,
        })
        .count();
    assert_eq!(waivers, 4, "one waiver note per round");

    // Release the gate so no agent is left sleeping in a stranded VM.
    let _ = std::fs::write(&gate, "");
}
