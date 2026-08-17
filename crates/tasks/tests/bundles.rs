//! Preserved bundles over HTTP, and the retention policy that empties the
//! directory.
//!
//! Real store, real files, real server on loopback. The bundles are token
//! byte strings rather than git bundles — nothing here runs git, because what
//! is under test is the listing, the authority and the predicate. That a
//! preserved bundle actually reconstructs the implementation is
//! `tests/builder.rs::a_rejected_egress_preserves_the_commits_and_the_command_recovers_them`,
//! which runs the printed recovery command against a fresh clone.

use std::sync::Arc;

use chrono::Utc;
use tasks::bundles::RejectedBundles;
use tasks::events::EventPayload;
use tasks::models::{
    Build, Complexity, DecisionInput, GhState, Project, ProjectId, ProjectStatus, Spec, SpecId,
    SpecQueueEntry, SpecQueueStatus, Task, TaskId, TaskState,
};
use tasks::store::Store;

struct Harness {
    store: Arc<Store>,
    bundles: RejectedBundles,
    base: String,
    project: Project,
    http: reqwest::Client,
    _tmp: tempfile::TempDir,
}

impl Harness {
    /// The orchestrator's own header. Everything else in these tests speaks
    /// as the human, who proves nothing and is never gated.
    fn as_orchestrator(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(
            "X-Tasks-Actor",
            format!("orchestrator {}", self.store.actor_token()),
        )
    }
}

/// A server with a bundle directory. `with_service = false` builds the router
/// without one, which is how the 503 is asserted.
async fn harness(with_service: bool) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "test".into(),
        repo_name: "repo".into(),
        added_at: Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();

    let bundles = RejectedBundles::under(tmp.path().join("build-scratch"));
    let app = tasks::server::router_with_services(
        store.clone(),
        tasks::server::Services {
            bundles: with_service.then(|| Arc::new(bundles.clone())),
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Harness {
        store,
        bundles,
        base,
        project,
        http: reqwest::Client::new(),
        _tmp: tmp,
    }
}

/// A task with an approved spec, ready to be built.
async fn seed_spec(store: &Store, project: &Project, issue: u64) -> (Task, Spec) {
    let now = Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: issue,
        title: format!("Issue {issue}"),
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

    let spec = Spec {
        id: SpecId::new(),
        session_id: None,
        task_id: task.id.clone(),
        content: format!("## Spec for {issue}"),
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

/// A build over `spec_ids`, failed at egress, with its bundle on disk.
async fn failed_build_with_bundle(h: &Harness, spec_ids: &[SpecId]) -> Build {
    let build = h
        .store
        .create_build(spec_ids, "main", DecisionInput::human())
        .await
        .unwrap();
    h.store.claim_next_queued_build().await.unwrap();
    h.store
        .set_build_base_sha(&build.id, "b45e5ba")
        .await
        .unwrap();
    h.bundles
        .preserve(&build.id, b"PACK the implementation")
        .await
        .unwrap();
    h.store
        .finalize_build_failed(
            &build.id,
            "branch egress: git push exited with 1; the build's commits were kept",
        )
        .await
        .unwrap();
    h.store.get_build(&build.id).await.unwrap().unwrap()
}

/// A later build over the same specs that succeeded, and whose tasks then
/// closed upstream — which is what "shipped" means here.
async fn rebuild_and_ship(h: &Harness, spec_ids: &[SpecId], pr: u64) -> Build {
    // The specs went to `built` when the first build failed only if it
    // succeeded; a failed one leaves them approved, so this is a plain rebuild.
    let build = h
        .store
        .create_build(spec_ids, "main", DecisionInput::human())
        .await
        .unwrap();
    h.store.claim_next_queued_build().await.unwrap();
    h.store
        .finalize_build_succeeded(&build.id, "deadbeef", pr, Some("summary"), &[])
        .await
        .unwrap();
    h.store.get_build(&build.id).await.unwrap().unwrap()
}

async fn ship_tasks(store: &Store, tasks: &[&Task]) {
    for task in tasks {
        store
            .update_task_state(&task.id, TaskState::Done)
            .await
            .unwrap();
    }
}

// --- the API ---

/// The ordinary state of a server that has never had an egress fail: the
/// directory does not exist, and that is an empty list rather than an error.
#[tokio::test]
async fn nothing_preserved_is_an_empty_list_and_a_404() {
    let h = harness(true).await;
    let (_task, spec) = seed_spec(&h.store, &h.project, 7).await;
    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();

    let listed: Vec<serde_json::Value> = h
        .http
        .get(format!("{}/bundles", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.is_empty(), "{listed:?}");

    let response = h
        .http
        .get(format!("{}/builds/{}/bundle", h.base, build.id))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

/// A router built without the bundle service has not looked in the directory,
/// so it must not answer as though it had. `[]` would say "nothing was
/// preserved", which is the one wrong answer to give about work that exists
/// in exactly one place.
#[tokio::test]
async fn a_server_without_the_service_says_so_rather_than_answering_empty() {
    let h = harness(false).await;
    for path in ["/bundles", "/builds/build_x/bundle"] {
        let response = h
            .http
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503, "{path}");
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("not the same as"),
            "{body}"
        );
    }
    let response = h
        .http
        .delete(format!("{}/builds/build_x/bundle", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
}

/// The listing is keyed to the *work*, not to a build id nobody recognises: a
/// build that never landed a branch has no PR and appears nowhere else.
#[tokio::test]
async fn a_preserved_bundle_is_reported_against_its_tasks_with_a_runnable_command() {
    let h = harness(true).await;
    let (task_a, spec_a) = seed_spec(&h.store, &h.project, 7).await;
    let (task_b, spec_b) = seed_spec(&h.store, &h.project, 9).await;
    let build = failed_build_with_bundle(&h, &[spec_a.id.clone(), spec_b.id.clone()]).await;

    let listed: Vec<serde_json::Value> = h
        .http
        .get(format!("{}/bundles", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    let bundle = &listed[0];
    assert_eq!(bundle["build_id"], build.id.to_string());
    assert_eq!(bundle["bytes"], 23);
    assert_eq!(bundle["branch"], build.branch);
    assert_eq!(bundle["base_sha"], "b45e5ba");
    assert_eq!(bundle["superseded"], false);
    assert!(
        bundle["exit_reason"]
            .as_str()
            .unwrap()
            .contains("git push exited"),
        "{bundle}"
    );
    let task_ids: Vec<String> = bundle["task_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        task_ids,
        vec![task_a.id.to_string(), task_b.id.to_string()],
        "both tasks, in batch order"
    );

    // The command names the file on disk and the branch to reconstruct — the
    // whole recovery, pasteable, because it runs on the server host and not
    // in the app.
    let command = bundle["recovery_command"].as_str().unwrap();
    let path = bundle["path"].as_str().unwrap();
    assert!(command.starts_with("git fetch "), "{command}");
    assert!(command.contains(path), "{command}");
    assert!(
        command.contains(&format!("{b}:{b}", b = build.branch)),
        "{command}"
    );
    assert!(tokio::fs::metadata(path).await.unwrap().is_file());

    // The per-build read answers the same thing.
    let one: serde_json::Value = h
        .http
        .get(format!("{}/builds/{}/bundle", h.base, build.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(&one, bundle);
}

/// Deleting one is the human's alone, and refused to the orchestrator
/// outright rather than charter-gated: what survives the retention policy is
/// by definition the only copy of something nobody reproduced.
#[tokio::test]
async fn the_orchestrator_may_not_throw_an_implementation_away() {
    let h = harness(true).await;
    let (_task, spec) = seed_spec(&h.store, &h.project, 7).await;
    let build = failed_build_with_bundle(&h, std::slice::from_ref(&spec.id)).await;

    let response = h
        .as_orchestrator(
            h.http
                .delete(format!("{}/builds/{}/bundle", h.base, build.id)),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("only copy"),
        "{body}"
    );
    assert!(
        h.bundles.stat(&build.id).await.unwrap().is_some(),
        "the refusal must not have deleted it anyway"
    );
}

/// The human's delete: 204, gone, recorded — and the record says the work had
/// *not* shipped, which is the difference between bookkeeping and a loss.
#[tokio::test]
async fn a_human_delete_is_recorded_as_the_loss_it_is() {
    let h = harness(true).await;
    let (_task, spec) = seed_spec(&h.store, &h.project, 7).await;
    let build = failed_build_with_bundle(&h, std::slice::from_ref(&spec.id)).await;

    let response = h
        .http
        .delete(format!("{}/builds/{}/bundle", h.base, build.id))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(h.bundles.stat(&build.id).await.unwrap().is_none());

    let removed = h
        .store
        .events_since(0, 100)
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::BundleRemoved {
                build_id,
                superseded,
                actor,
            } => Some((build_id, superseded, actor)),
            _ => None,
        })
        .expect("a removal was recorded");
    assert_eq!(removed.0, build.id);
    assert!(!removed.1, "nothing had rebuilt this");
    assert_eq!(removed.2, tasks::models::Actor::Human);

    // A second click, or a reclaim that got there first: honest, not an error
    // shaped like a bug.
    let again = h
        .http
        .delete(format!("{}/builds/{}/bundle", h.base, build.id))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
}

// --- the retention policy ---

/// Nothing rebuilt it: kept, forever, and by design. Age is never a reason.
#[tokio::test]
async fn a_bundle_nobody_rebuilt_is_kept() {
    let h = harness(true).await;
    let (_task, spec) = seed_spec(&h.store, &h.project, 7).await;
    let build = failed_build_with_bundle(&h, std::slice::from_ref(&spec.id)).await;

    assert!(!h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(h.bundles.stat(&build.id).await.unwrap().is_some());
}

/// A later build that only opened a PR is **not** evidence. `watch_merges` can
/// still find that PR closed unmerged and unwind the batch back to
/// `ready_to_build`, at which point this bundle is the head start again — so
/// the predicate wants `done` (the issue closed upstream) and not
/// `succeeded` alone.
#[tokio::test]
async fn a_bundle_is_kept_while_the_rebuild_has_only_opened_a_pr() {
    let h = harness(true).await;
    let (task, spec) = seed_spec(&h.store, &h.project, 7).await;
    let build = failed_build_with_bundle(&h, std::slice::from_ref(&spec.id)).await;

    let later = rebuild_and_ship(&h, std::slice::from_ref(&spec.id), 42).await;
    assert_eq!(later.status, tasks::models::BuildStatus::Succeeded);
    assert_eq!(
        h.store.get_task(&task.id).await.unwrap().unwrap().state,
        TaskState::AwaitingMerge,
        "a PR is a claim, not a delivery"
    );

    assert!(!h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(h.bundles.stat(&build.id).await.unwrap().is_some());

    // And once the issue closes upstream — which is the only thing that
    // writes `done` — it goes.
    ship_tasks(&h.store, &[&task]).await;
    assert!(h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(h.bundles.stat(&build.id).await.unwrap().is_none());
}

/// One unreproduced spec keeps the whole bundle. A bundle is one file over a
/// whole batch; there is no half-bundle to keep.
#[tokio::test]
async fn one_unreproduced_spec_keeps_the_whole_bundle() {
    let h = harness(true).await;
    let (task_a, spec_a) = seed_spec(&h.store, &h.project, 7).await;
    let (task_b, spec_b) = seed_spec(&h.store, &h.project, 9).await;
    let build = failed_build_with_bundle(&h, &[spec_a.id.clone(), spec_b.id.clone()]).await;

    // Only the first half was rebuilt, and it shipped.
    rebuild_and_ship(&h, std::slice::from_ref(&spec_a.id), 42).await;
    ship_tasks(&h.store, &[&task_a]).await;

    assert!(!h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(
        h.bundles.stat(&build.id).await.unwrap().is_some(),
        "spec_b was never rebuilt"
    );

    // The rest of the batch, and now it is redundant.
    rebuild_and_ship(&h, std::slice::from_ref(&spec_b.id), 43).await;
    ship_tasks(&h.store, &[&task_b]).await;
    assert!(h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(h.bundles.stat(&build.id).await.unwrap().is_none());

    let reclaimed = h
        .store
        .events_since(0, 500)
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::BundleRemoved {
                superseded, actor, ..
            } => Some((superseded, actor)),
            _ => None,
        })
        .expect("the reclaim was recorded");
    assert!(reclaimed.0, "this one had shipped");
    assert_eq!(
        reclaimed.1,
        tasks::models::Actor::System,
        "the server acting on a fact, not a judgment anybody made"
    );
}

/// "Later" is insertion order, never `created_at`. Two builds stamped inside
/// the same second would otherwise let a build supersede *itself* — the
/// bundle deleted on the strength of the very run that failed to push it.
#[tokio::test]
async fn a_build_cannot_supersede_itself() {
    let h = harness(true).await;
    let (task, spec) = seed_spec(&h.store, &h.project, 7).await;

    // Succeeded rather than failed, so `status = 'succeeded'` matches and the
    // only thing standing between this build and its own bundle is the rowid
    // comparison.
    let build = h
        .store
        .create_build(
            std::slice::from_ref(&spec.id),
            "main",
            DecisionInput::human(),
        )
        .await
        .unwrap();
    h.store.claim_next_queued_build().await.unwrap();
    h.store
        .finalize_build_succeeded(&build.id, "deadbeef", 42, None, &[])
        .await
        .unwrap();
    h.bundles.preserve(&build.id, b"PACK").await.unwrap();
    ship_tasks(&h.store, &[&task]).await;

    assert!(!h.store.build_superseded(&build.id).await.unwrap());
    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(h.bundles.stat(&build.id).await.unwrap().is_some());
}

/// A bundle whose build row is gone has no branch and no base, so there is no
/// honest recovery command to print and no way to show it was reproduced. It
/// is left on disk and left out of the listing — the safe direction, since
/// deleting it would be deleting an implementation on the strength of a
/// missing row.
#[tokio::test]
async fn a_bundle_with_no_build_row_is_left_alone() {
    let h = harness(true).await;
    let orphan = tasks::models::BuildId::from_raw("build_vanished");
    h.bundles.preserve(&orphan, b"PACK").await.unwrap();

    let listed: Vec<serde_json::Value> = h
        .http
        .get(format!("{}/bundles", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.is_empty(), "{listed:?}");

    let response = h
        .http
        .get(format!("{}/builds/{orphan}/bundle", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    tasks::run::reclaim_bundles(&h.store, &h.bundles).await;
    assert!(
        h.bundles.stat(&orphan).await.unwrap().is_some(),
        "an unexplainable bundle is still an implementation"
    );
}
