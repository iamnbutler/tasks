//! Worker runs (#1053): labor out of the orchestrator's conversation lane.
//!
//! The properties under test, in the order they matter:
//!
//! - **Every ending becomes a report.** A worker that succeeds, fails, is
//!   cancelled or dies must land a server-written `[worker <job>]` event turn
//!   in the conversation — silence is the one outcome the design forbids.
//! - **A worker conveys labor, not authority.** The dispatch is charter-gated
//!   with a mandatory rationale for the orchestrator, `shadow` records and
//!   runs nothing, and the human is never gated.
//! - **Output streams.** A failing worker's report carries what it had said
//!   before it died, and its transcript is persisted line by line.
//!
//! Real store, real child processes standing in for headless Claude Code,
//! real HTTP server. No mocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tasks::models::{
    Actor, Capability, CharterLevel, ChatRole, RunKind, TranscriptOwner, WorkerId, WorkerStatus,
};
use tasks::run::worker_loop;
use tasks::store::Store;
use tokio::sync::watch;

mod common;

struct Harness {
    store: Arc<Store>,
    base: String,
    http: reqwest::Client,
}

impl Harness {
    fn as_orchestrator(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.header(
            "X-Tasks-Actor",
            format!("orchestrator {}", self.store.actor_token().expose()),
        )
    }

    async fn conversation(&self) -> Vec<tasks::models::OrchestratorMessage> {
        self.store
            .orchestrator_messages_since(0, 100)
            .await
            .unwrap()
    }
}

async fn harness() -> Harness {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness {
        store,
        base,
        http: reqwest::Client::new(),
    }
}

/// A stub worker agent: prints `lines` (one per second-ish burst, no delay),
/// then exits with `code`. Plain text on purpose — a worker whose agent
/// doesn't speak stream-json must still report, and the raw path is the one
/// test stubs exercise everywhere else in this suite.
async fn write_stub(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let stub = dir.join(name);
    tokio::fs::write(&stub, format!("#!/bin/sh\ncat > /dev/null\n{body}"))
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = tokio::fs::metadata(&stub).await.unwrap().permissions();
        p.set_mode(0o755);
        tokio::fs::set_permissions(&stub, p).await.unwrap();
    }
    stub
}

/// Spawn the worker lane against a stub command; returns the shutdown handle.
fn spawn_lane(store: &Arc<Store>, data_dir: &Path, cmd: String) -> watch::Sender<bool> {
    let mut config = common::offline_config(data_dir);
    config.worker_cmd = cmd;
    config.worker_timeout = Duration::from_secs(30);
    let (tx, rx) = watch::channel(false);
    tokio::spawn(worker_loop(store.clone(), config, None, rx));
    tx
}

async fn wait_for_status(store: &Store, id: &WorkerId, want: WorkerStatus) {
    for _ in 0..300 {
        let worker = store.worker(id).await.unwrap().unwrap();
        if worker.status == want {
            return;
        }
        assert!(
            !worker.status.is_terminal(),
            "worker concluded {} while waiting for {}: {:?}",
            worker.status,
            want.as_str(),
            worker.exit_reason
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("worker never reached {}", want.as_str());
}

#[tokio::test]
async fn a_worker_runs_and_its_report_lands_as_a_worker_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let h = harness().await;
    let stub = write_stub(
        tmp.path(),
        "worker-ok.sh",
        "echo 'checked out the branch'\necho 'suite: 991 passed'\n",
    )
    .await;
    let _lane = spawn_lane(&h.store, tmp.path(), stub.display().to_string());

    let resp = h
        .http
        .post(format!("{}/workers", h.base))
        .json(&json!({ "job": "verify PR 1063", "prompt": "run the suite on PR 1063" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "a human dispatch is never gated");
    let worker: Value = resp.json().await.unwrap();
    let id = WorkerId::from_raw(worker["id"].as_str().unwrap().to_string());

    wait_for_status(&h.store, &id, WorkerStatus::Succeeded).await;
    let row = h.store.worker(&id).await.unwrap().unwrap();
    assert!(
        row.report.as_deref().unwrap().contains("991 passed"),
        "the stub's output is the report: {:?}",
        row.report
    );

    // The report turn: server-written heading, event role, the report inside.
    let messages = h.conversation().await;
    let report = messages
        .iter()
        .find(|m| m.content.starts_with("[worker verify PR 1063]"))
        .expect("a report turn landed");
    assert_eq!(report.role, ChatRole::Event);
    assert!(report.content.contains("991 passed"), "{}", report.content);
    assert!(
        report.content.contains(id.as_str()),
        "the turn names the run: {}",
        report.content
    );

    // And the output streamed into a persisted transcript.
    let lines = h
        .store
        .transcript_since(&TranscriptOwner::worker(&id), 0, 100)
        .await
        .unwrap();
    assert!(
        lines.iter().any(|l| l.line.contains("checked out")),
        "the transcript holds what the worker said: {lines:?}"
    );

    // The API surfaces the run.
    let listed: Vec<Value> = h
        .http
        .get(format!("{}/workers", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.iter().any(|w| w["id"] == id.as_str()));
    let fetched: Vec<Value> = h
        .http
        .get(format!("{}/workers/{}/transcript", h.base, id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!fetched.is_empty(), "the transcript route answers");
}

#[tokio::test]
async fn a_failing_worker_reports_how_it_ended_and_what_it_streamed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = harness().await;
    let stub = write_stub(
        tmp.path(),
        "worker-dies.sh",
        "echo 'test 800 of 991: three failures so far'\nexit 1\n",
    )
    .await;
    let _lane = spawn_lane(&h.store, tmp.path(), stub.display().to_string());

    let worker = h
        .store
        .create_worker("composition", "run it")
        .await
        .unwrap();
    wait_for_status(&h.store, &worker.id, WorkerStatus::Failed).await;

    let row = h.store.worker(&worker.id).await.unwrap().unwrap();
    assert!(
        row.exit_reason.as_deref().unwrap().contains("exited with"),
        "{:?}",
        row.exit_reason
    );

    let messages = h.conversation().await;
    let report = messages
        .iter()
        .find(|m| m.content.starts_with("[worker composition]"))
        .expect("a dead worker still reports");
    assert!(
        report.content.contains("ended without completing"),
        "{}",
        report.content
    );
    // The salvage: what it had streamed before it died.
    assert!(
        report.content.contains("three failures so far"),
        "the report carries what the run had, not just how it ended: {}",
        report.content
    );
    // No strike machinery: nothing says "attempt", nothing counts.
    assert!(
        report.content.contains("No attempt is charged"),
        "{}",
        report.content
    );
}

#[tokio::test]
async fn a_running_worker_is_cancelled_through_the_durable_row() {
    let tmp = tempfile::tempdir().unwrap();
    let h = harness().await;
    let stub = write_stub(tmp.path(), "worker-slow.sh", "sleep 30\n").await;
    let _lane = spawn_lane(&h.store, tmp.path(), stub.display().to_string());

    let worker = h.store.create_worker("slow job", "wait").await.unwrap();
    wait_for_status(&h.store, &worker.id, WorkerStatus::Running).await;

    let resp = h
        .http
        .post(format!("{}/workers/{}/cancel", h.base, worker.id))
        .json(&json!({ "rationale": "wrong job" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    wait_for_status(&h.store, &worker.id, WorkerStatus::Cancelled).await;
    let row = h.store.worker(&worker.id).await.unwrap().unwrap();
    assert!(
        row.exit_reason.as_deref().unwrap().contains("wrong job"),
        "the rationale lands in the exit reason: {:?}",
        row.exit_reason
    );
    // A cancel is an ending too, and endings report.
    let messages = h.conversation().await;
    assert!(
        messages
            .iter()
            .any(|m| m.content.starts_with("[worker slow job]")),
        "a cancelled worker still lands a turn"
    );
}

#[tokio::test]
async fn a_queued_worker_cancel_applies_immediately_with_no_lane_running() {
    let h = harness().await;
    let worker = h
        .store
        .create_worker("parked", "never claimed")
        .await
        .unwrap();

    let resp = h
        .http
        .post(format!("{}/workers/{}/cancel", h.base, worker.id))
        .json(&json!({ "rationale": "changed my mind" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: Value = resp.json().await.unwrap();
    assert_eq!(ack["run_kind"], "worker");
    assert_eq!(
        ack["concluded"], true,
        "nothing is following a queued worker, so the handler applies it: {ack}"
    );

    let row = h.store.worker(&worker.id).await.unwrap().unwrap();
    assert_eq!(row.status, WorkerStatus::Cancelled);

    // And cancelling a concluded run is a conflict, not a second cancel.
    let resp = h
        .http
        .post(format!("{}/workers/{}/cancel", h.base, worker.id))
        .json(&json!({ "rationale": "again" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn dispatch_is_charter_gated_with_a_rationale_and_shadow_runs_nothing() {
    let h = harness().await;

    // Off: the orchestrator is refused outright; the human is never gated.
    h.store
        .set_charter(Capability::DispatchWorkers, CharterLevel::Off, None)
        .await
        .unwrap();
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/workers", h.base)))
        .json(&json!({ "job": "j", "prompt": "p", "rationale": "why" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let resp = h
        .http
        .post(format!("{}/workers", h.base))
        .json(&json!({ "job": "human job", "prompt": "p" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "the human is never gated");

    // Live without a rationale: refused before anything exists.
    h.store
        .set_charter(Capability::DispatchWorkers, CharterLevel::Live, None)
        .await
        .unwrap();
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/workers", h.base)))
        .json(&json!({ "job": "j", "prompt": "p" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "a dispatch with no rationale is refused"
    );

    // Live with one: dispatched and ledgered.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/workers", h.base)))
        .json(&json!({ "job": "verify", "prompt": "p", "rationale": "the batch is parked" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let worker: Value = resp.json().await.unwrap();
    let decisions = h.store.decisions(None, 10).await.unwrap();
    let row = decisions
        .iter()
        .find(|d| d.subject_id == worker["id"].as_str().unwrap())
        .expect("the dispatch is ledgered");
    assert_eq!(row.actor, Actor::Orchestrator);

    // Shadow: recorded, nothing dispatched.
    h.store
        .set_charter(Capability::DispatchWorkers, CharterLevel::Shadow, None)
        .await
        .unwrap();
    let before = h.store.list_workers(100).await.unwrap().len();
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/workers", h.base)))
        .json(&json!({ "job": "shadowed", "prompt": "p", "rationale": "why" }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["shadowed"], true, "{body}");
    assert_eq!(
        h.store.list_workers(100).await.unwrap().len(),
        before,
        "a shadowed dispatch creates no worker"
    );
}

#[tokio::test]
async fn a_job_label_that_could_forge_a_heading_is_refused() {
    let h = harness().await;
    for job in ["two\nlines", "with [brackets]", "", "   "] {
        let resp = h
            .http
            .post(format!("{}/workers", h.base))
            .json(&json!({ "job": job, "prompt": "p" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "job {job:?} should be refused");
    }
}

#[tokio::test]
async fn the_lane_is_serial_and_orphans_are_written_off_as_reports() {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let first = store.create_worker("first", "p").await.unwrap();
    let second = store.create_worker("second", "p").await.unwrap();

    let claimed = store.claim_next_queued_worker().await.unwrap().unwrap();
    assert_eq!(claimed.id, first.id, "oldest first");
    assert!(
        store.claim_next_queued_worker().await.unwrap().is_none(),
        "one at a time: the lane is serial"
    );

    // A dead process left one running and one queued; both are written off,
    // returned for reporting, and nothing is left for the claim.
    let orphans = store
        .reconcile_orphaned_workers("orphaned by server restart")
        .await
        .unwrap();
    let ids: Vec<_> = orphans.iter().map(|w| w.id.clone()).collect();
    assert!(
        ids.contains(&first.id) && ids.contains(&second.id),
        "{ids:?}"
    );
    for worker in [&first.id, &second.id] {
        let row = store.worker(worker).await.unwrap().unwrap();
        assert_eq!(row.status, WorkerStatus::Failed);
        assert_eq!(
            row.exit_reason.as_deref(),
            Some("orphaned by server restart")
        );
    }
    assert!(store.claim_next_queued_worker().await.unwrap().is_none());
}

#[tokio::test]
async fn a_cancel_for_a_worker_names_the_worker_kind() {
    // The three-kind cancel plumbing: a worker cancel is `RunKind::Worker` on
    // the durable row, so the observer keyed (kind, id) can never confuse it
    // with a session or build sharing an id suffix.
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    store
        .request_cancel(
            RunKind::Worker,
            "work_1",
            Actor::Human,
            Some("enough"),
            None,
        )
        .await
        .unwrap();
    let pending = store
        .pending_cancel(RunKind::Worker, "work_1")
        .await
        .unwrap()
        .expect("the request is durable");
    assert_eq!(pending.exit_reason(), "cancelled by human: enough");
    assert!(
        store
            .pending_cancel(RunKind::Build, "work_1")
            .await
            .unwrap()
            .is_none(),
        "a worker cancel is not a build cancel"
    );
}
