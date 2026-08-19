//! What the scout dispatcher may start, and whether it may start it — asked
//! together, so the second question cannot be dropped from the first (#973).
//!
//! [`top_up`](crate::run::top_up) reads the four standing dispatch holds twice:
//! once before its loop, for cost, and once **per scout**, because each
//! iteration starts a VM and a pause landing mid-pass must stop the next one
//! rather than merely the next pass (#948). That per-scout read is correct and
//! was invisible to the suite: deleting it left every test green, because the
//! only thing that could observe it is a window with nothing awaited in it —
//! which is exactly what makes the fix correct. A test of the predicate cannot
//! see a deleted call site.
//!
//! So the rule is pinned structurally instead, on the [`crate::server::ledgered`]
//! precedent. [`next_scout`] answers both questions in one call and the only
//! thing it hands back is a [`Cleared`], whose fields are private to this
//! module; [`next_dispatchable`] — the half that answers "what is next" without
//! asking "may I" — is private too. A pass that starts a VM without re-reading
//! the holds therefore has no route to a `(Task, Project)` at all: it cannot be
//! written, rather than being written and caught.
//!
//! Making `next_dispatchable` private is the load-bearing half. Had
//! [`next_scout`] been added *beside* it, a refactor could call the old one and
//! go green again — which is the failure this module exists to prevent, one
//! level up.
//!
//! Two unit tests below pin the two properties, one mutation each: deleting the
//! read inside [`next_scout`], and moving it back in front of the scan.

use std::collections::HashSet;

use tracing::warn;
use vm_pool_client::ClientHandle;

use crate::events::EventPayload;
use crate::github_health::GitHubHealth;
use crate::models::{GhState, Mode, Project, Task, TaskId, TaskState};
use crate::protocol::TasksProtocol;
use crate::run::{DISPATCHER, MAX_DISPATCH_ATTEMPTS, github_hold};
use crate::store::{Store, StoreError};

/// A task the dispatcher may start a VM for, *right now*.
///
/// The fields are private and there is exactly one constructor — inside
/// [`next_scout`], after the hold read — so holding one of these **is** the
/// evidence that the four holds were re-read since the queue was scanned. That
/// is the whole mechanism: the caller cannot assemble one, and cannot reach the
/// pair any other way, because [`next_dispatchable`] does not leave this module.
pub struct Cleared {
    task: Task,
    project: Project,
}

impl Cleared {
    /// Spend the clearance. Consuming, so it reads as the one-shot permission
    /// it is rather than as an accessor a loop could call twice.
    pub fn into_parts(self) -> (Task, Project) {
        (self.task, self.project)
    }
}

/// The answer to "start another scout?" — and *why not*, when the answer is no.
///
/// The caller breaks on both refusals, so the distinction is not control flow;
/// it is the reason an idle pipeline can say which kind of idle it is, which is
/// a question this codebase already answers in three other places
/// (`/status`, `tasks status`, the event feed). [`crate::run::top_up`] logs it.
/// It is also what gives the ordering rule something to fail on: collapsing
/// this to an `Option<Cleared>` would leave "the holds are read *after* the
/// scan" unpinned again, because a hold read first answers `Held` for an empty
/// queue and nothing could tell.
#[expect(
    clippy::large_enum_variant,
    reason = "a Task is ~296 bytes; exactly one of these exists at a time, on \
              the stack, for the few lines between the scan and the spawn"
)]
pub enum NextScout {
    /// Dispatch this one.
    Start(Cleared),
    /// Something is holding new dispatches. There may well be work waiting.
    Held,
    /// Nothing eligible is left in the queue.
    Drained,
}

/// The next scout to start, if the queue has one and nothing is holding
/// dispatch.
///
/// **The scan runs first and the holds are read after it**, and that order is
/// the point rather than an implementation detail. A human pauses and *then*
/// queues work, so anything the scan could see was committed after the pause
/// was, and a read that follows the scan cannot miss it. Read before the scan
/// and that window reopens. It is also the last thing before the caller's
/// `spawn`, with nothing awaited in between — which is what makes it a
/// per-dispatch read rather than a per-pass one.
pub async fn next_scout(
    store: &Store,
    health: &GitHubHealth,
    updates: &crate::updates::UpdateWatch,
    pool_health: &crate::pool_health::PoolHealth,
    handle: &ClientHandle<TasksProtocol>,
    skip: &HashSet<TaskId>,
) -> Result<NextScout, StoreError> {
    let Some((task, project)) = next_dispatchable(store, skip).await? else {
        return Ok(NextScout::Drained);
    };

    // Asked again, per scout, because each iteration starts a VM and a pause
    // landing mid-pass must stop the next one — not merely the next pass
    // (#948). Deleting this line is the mutation
    // `a_hold_that_lands_after_the_pass_began_stops_the_next_scout` kills;
    // hoisting it above the scan is the one
    // `the_holds_are_read_after_the_scan_not_before_it` kills.
    if dispatch_held(store, health, updates, pool_health, handle).await? {
        return Ok(NextScout::Held);
    }

    Ok(NextScout::Start(Cleared { task, project }))
}

/// Whether new scouts must wait: the four standing reasons, asked together.
///
/// One function rather than four inline checks so a fifth reason cannot be
/// added at one call site and forgotten at the other — [`crate::run::top_up`]
/// asks once before its loop (for cost) and [`next_scout`] once per dispatch
/// (for freshness), and a new hold belongs here as one more early
/// `return Ok(true)` rather than at either call site.
///
/// Silent by design, like the checks it replaces: a pause is the human's own
/// act, the GitHub edge is announced by the poller, the update edge by the
/// watch and the pool edge by [`announce_pool`], and `/status` answers for
/// whoever asks later. A 500 ms loop that logged its refusals is what trains a
/// reader to ignore them.
///
/// The build lane asks the same four questions in its own match guard (see
/// [`crate::run::build_loop`]) and deliberately does **not** call this: it
/// claims at most one build per pass, so it already re-reads them for every
/// container it starts, and sharing this would mean restructuring a match guard
/// around an `await`. The comment there points back here, so whichever site is
/// edited names the other.
pub async fn dispatch_held(
    store: &Store,
    health: &GitHubHealth,
    updates: &crate::updates::UpdateWatch,
    pool_health: &crate::pool_health::PoolHealth,
    handle: &ClientHandle<TasksProtocol>,
) -> Result<bool, StoreError> {
    if store.get_mode().await? != Mode::Play {
        return Ok(true);
    }

    // A stop before the queue, not a filter over it: an outage is a fact about
    // the world, not about any one task, and skipping held work to find
    // something else to dispatch would just pick a different victim.
    if github_hold(health) {
        return Ok(true);
    }

    // Same shape for a half-applied upgrade: a new scout would run in the
    // stale half of it. Silent here for the same reason as the GitHub hold —
    // the transition is announced by the watch itself, once, and `/status`
    // answers for whoever asks later.
    if updates.hold(store).await {
        return Ok(true);
    }

    // And the pool itself: a dispatch into a full pool is refused, and a
    // refused Scout is one whose task has to be unwound back to `Queued` —
    // which, without this, the 500 ms tick would re-attempt twice a second for
    // as long as the pool stayed full (#967). Unlike the two above, this one
    // does its own observing: the probe is claimed at most once per
    // `PROBE_INTERVAL` across both gates, so asking here costs at most one
    // local round trip every five seconds.
    if pool_hold(pool_health, handle, store).await {
        return Ok(true);
    }

    Ok(false)
}

/// Refresh the capacity record if a probe is due, then read the hold.
///
/// The probe is a `status` round trip and never a classified refusal: the
/// quantity `Pool::allocate` checks is `available`, and a refusal reaches this
/// process as prose. It is also what breaks the circle — the natural clearing
/// signal for a refusal-driven record is a successful allocation, which is the
/// one thing a hold prevents.
///
/// `pub(crate)` for the build lane, which reads the holds in a match guard of
/// its own rather than through [`dispatch_held`].
pub(crate) async fn pool_hold(
    pool_health: &crate::pool_health::PoolHealth,
    handle: &ClientHandle<TasksProtocol>,
    store: &Store,
) -> bool {
    if pool_health.probe_due(chrono::Utc::now()) {
        let status = handle.status().await;
        let transition = pool_health.observe(&status, chrono::Utc::now());
        announce_pool(store, transition).await;
    }
    pool_health.hold(chrono::Utc::now()).is_some()
}

/// Say once, per edge, that the pool filled up or freed a slot — in the log
/// and on the event feed.
///
/// Driven by the [`Transition`](crate::pool_health::Transition) that
/// [`crate::pool_health::PoolHealth::observe`] returns under the probe claim,
/// and never by the `hold` predicate: two loops reading a held predicate every
/// tick would write a `Note` per tick, which is the event-log flood this change
/// exists to prevent, one level up. The claim is what makes exactly one of two
/// racing gates write it — the same rule the update watch's `announce` states
/// for its own mutex.
async fn announce_pool(store: &Store, transition: crate::pool_health::Transition) {
    use crate::pool_health::Transition;
    let message = match transition {
        Transition::Unchanged => return,
        Transition::Exhausted(run) => {
            let message = run.describe();
            warn!(total = run.total, "{message}");
            message
        }
        Transition::Freed(run) => {
            let message = format!(
                "vm-pool has a slot again (it was full for {}s, {} observation(s)); \
                 dispatch resumes",
                (run.last - run.since).num_seconds(),
                run.observations
            );
            tracing::info!("{message}");
            message
        }
    };
    if let Err(e) = store
        .append_event(EventPayload::Note {
            source: DISPATCHER.into(),
            message,
        })
        .await
    {
        warn!(error = %e, "could not record the vm-pool capacity hold on the feed");
    }
}

/// The next task to scout: queue order (which [`Store::list_tasks`] already
/// applies), state `Queued` (explicitly picked up), still open on GitHub, not in flight, not past the
/// attempt cap, and belonging to a project the dispatcher is still working on.
///
/// A task at the cap is rejected the moment it gets there, so the attempt
/// filter here is belt-and-braces: it also covers rows an older build (or a
/// crash between the increment and the rejection) left `Queued` at three strikes.
///
/// **Private to this module, deliberately.** This is the function that hands
/// out a `(Task, Project)`, and every route to one has to pass the hold read in
/// [`next_scout`]. Widening it back to `pub(crate)` for a caller's convenience
/// reopens exactly the hole #973 is about.
async fn next_dispatchable(
    store: &Store,
    skip: &HashSet<TaskId>,
) -> Result<Option<(Task, Project)>, StoreError> {
    for task in store.list_tasks().await? {
        if task.state != TaskState::Queued
            || task.gh_state == GhState::Closed
            || skip.contains(&task.id)
            || task.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS
        {
            continue;
        }
        let Some(project) = store.get_project(&task.project_id).await? else {
            warn!(task_id = %task.id, project_id = %task.project_id, "task references a missing project");
            continue;
        };
        // `continue`, not `break`: a paused repo at the head of the queue must
        // not starve the ones behind it — that is the whole difference between
        // pausing one repo and pausing the server.
        if !project.status.dispatches() {
            continue;
        }
        return Ok(Some((task, project)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use vm_pool_client::{Client, PoolStatus};

    use super::*;
    use crate::github::GhError;
    use crate::models::{ProjectId, ProjectStatus, TaskId};

    fn project() -> Project {
        Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "tasks".into(),
            added_at: Utc::now(),
            status: ProjectStatus::Active,
        }
    }

    async fn seed_task(store: &Store, project: &Project, number: u64, state: TaskState) -> Task {
        let now = Utc::now();
        let task = Task {
            id: TaskId::new(),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: format!("issue {number}"),
            body: String::new(),
            labels: vec![],
            gh_state: GhState::Open,
            state,
            priority: 0,
            manual_rank: (state == TaskState::Queued).then_some(1),
            dispatch_attempts: 0,
            ingested_at: now,
            updated_at: now,
            scout_directions: None,
        };
        store.insert_task(&task).await.unwrap();
        task
    }

    /// A 5xx — GitHub failing to *answer*, which is the only shape of failure
    /// that can put a hold on dispatch.
    fn unavailable() -> Result<(), GhError> {
        Err(GhError::Rest {
            what: "pull request".into(),
            status: reqwest::StatusCode::BAD_GATEWAY,
            message: "bad gateway".into(),
        })
    }

    /// A real vm-pool on a temp socket, with room — the pool hold is one of
    /// the four questions [`dispatch_held`] asks, and it asks it over a
    /// connection. No mocks, per the house rule; `NoRuntime` starts no
    /// containers, which is all this needs.
    async fn pool_with_room(
        dir: &std::path::Path,
        max_vms: usize,
    ) -> (
        std::sync::Arc<vm_pool_service::Service<vm_pool_manager::NoRuntime, TasksProtocol>>,
        Client<TasksProtocol>,
    ) {
        let config = vm_pool_service::ServiceConfig {
            socket_path: dir.join("vm-pool.sock"),
            snapshot_dir: dir.join("snapshots"),
            state_dir: dir.join("state"),
            pool: vm_pool_manager::PoolConfig {
                max_vms,
                health_check_interval: 60,
                vm_timeout: 300,
            },
        };
        let socket = config.socket_path.clone();
        let service =
            vm_pool_service::Service::<vm_pool_manager::NoRuntime, TasksProtocol>::new(config)
                .await
                .expect("service");
        let svc = service.clone();
        tokio::spawn(async move {
            let _ = svc.run().await;
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::<TasksProtocol>::connect(&socket)
            .await
            .expect("connect");
        (service, client)
    }

    /// A pool status with no free slot, folded straight into the record.
    ///
    /// The round trip is *not* the subject here — [`crate::pool_health`] owns
    /// that, and the probe is claimed at most once every five seconds, so a
    /// test that flipped a real pool's occupancy would be waiting on a clock
    /// rather than on the gate. Both legs below therefore spend the first
    /// probe against a pool with room and then write the record directly, so
    /// what they measure is `dispatch_held` reading it.
    fn full() -> Result<PoolStatus, vm_pool_client::ClientError> {
        Ok(PoolStatus {
            total: 4,
            available: 0,
            allocated: 4,
            protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
        })
    }

    fn has_room() -> Result<PoolStatus, vm_pool_client::ClientError> {
        Ok(PoolStatus {
            total: 4,
            available: 3,
            allocated: 1,
            protocol_version: vm_pool_protocol::PROTOCOL_VERSION,
        })
    }

    /// The mutation killer: delete the [`dispatch_held`] call inside
    /// [`next_scout`] and this fails, while every other test in the file stays
    /// green — which is #973 reproduced as an experiment.
    ///
    /// It replays #948's own scenario rather than a predicate in isolation.
    /// Two queued tasks and room for both, so `top_up`'s loop runs twice and
    /// its *pre-loop* read has already said yes; the hold lands between the
    /// two turns. Nothing about a pass is faked: the first `Start` is taken and
    /// reserved in `skip` exactly as `top_up` does with `in_flight_ids`.
    ///
    /// All four reasons get a leg, each with its release, because
    /// [`dispatch_held`] is the one place they live and a leg that quietly went
    /// missing would leave that reason unpinned at the call site.
    #[tokio::test]
    async fn a_hold_that_lands_after_the_pass_began_stops_the_next_scout() {
        let store = Store::open_in_memory().await.unwrap();
        let health = GitHubHealth::default();
        let updates = crate::updates::UpdateWatch::at_boot(true);
        let dir = tempfile::tempdir().unwrap();
        let (_svc, client) = pool_with_room(dir.path(), 4).await;
        let pool_health = crate::pool_health::PoolHealth::new();
        let handle = client.handle();

        let project = project();
        store.insert_project(&project).await.unwrap();
        let first = seed_task(&store, &project, 1, TaskState::Queued).await;
        let second = seed_task(&store, &project, 2, TaskState::Queued).await;
        store
            .set_queue_order(&[first.id.clone(), second.id.clone()])
            .await
            .unwrap();
        store.set_mode(Mode::Play).await.unwrap();

        // Whatever the first turn of a pass does, the second turn must ask
        // again — so each leg starts from a clean, dispatching state.
        let mut skip: HashSet<TaskId> = HashSet::new();
        let turn_one = next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
            .await
            .unwrap();
        let NextScout::Start(cleared) = turn_one else {
            panic!("nothing is holding dispatch: the first scout of the pass must start");
        };
        let (task, _project) = cleared.into_parts();
        assert_eq!(task.id, first.id);
        skip.insert(task.id.clone());

        // --- 1. A pause committed after the pass began. ---
        store.set_mode(Mode::Pause).await.unwrap();
        assert!(
            matches!(
                next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Held
            ),
            "the second scout of the same pass must see the pause (#948)"
        );
        assert_eq!(
            store.get_task(&second.id).await.unwrap().unwrap().state,
            TaskState::Queued,
            "and the task it did not start stays queued, not rejected"
        );
        store.set_mode(Mode::Stop).await.unwrap();
        assert!(
            matches!(
                next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Held
            ),
            "and so must a stop"
        );
        store.set_mode(Mode::Play).await.unwrap();

        // --- 2. A GitHub outage that started mid-pass, and its recovery. ---
        let now = Utc::now();
        health.observe(&unavailable(), now);
        assert!(
            matches!(
                next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Held
            ),
            "a 5xx observed mid-pass holds the next scout of that pass"
        );
        health.observe(&Ok::<(), GhError>(()), now);

        // --- 3. A stale image, observed by the scout this pass just started. ---
        // The one hold a *dispatch* can cause: `images::observe` records an
        // `Unstamped` identity stamped now, which both `needs_rebuild()` and
        // post-dates this watch's boot, so the watch holds on it.
        crate::images::observe(
            &store,
            "agent:v1",
            tasks_api::version::ImageRole::Scout,
            None,
            "run-1",
        )
        .await;
        assert!(
            matches!(
                next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Held
            ),
            "an image observed stale by this pass's own first scout holds its second"
        );
        // Released the only way it can be: a watch that is no longer enforcing.
        let released = crate::updates::UpdateWatch::at_boot(false);

        // --- 4. A pool that filled up mid-pass, and a slot coming back. ---
        // The probe above is spent, so the record is what the next read sees.
        pool_health.observe(&full(), Utc::now());
        assert!(
            matches!(
                next_scout(&store, &health, &released, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Held
            ),
            "a pool that filled up mid-pass holds the next scout of that pass (#967)"
        );
        pool_health.observe(&has_room(), Utc::now());

        // Every hold released, and the queue still has the task nothing started.
        let resumed = next_scout(&store, &health, &released, &pool_health, &handle, &skip)
            .await
            .unwrap();
        let NextScout::Start(cleared) = resumed else {
            panic!("with every hold released the second scout starts");
        };
        assert_eq!(cleared.into_parts().0.id, second.id);
    }

    /// The ordering: **scan, then holds**. Move the [`dispatch_held`] call back
    /// above [`next_dispatchable`] — the pre-#948 shape — and this fails while
    /// the other three pass.
    ///
    /// Only a scan that ran first can say the queue is empty, so `Drained`
    /// under a live hold is the whole assertion. This is also what `Held` vs
    /// `Drained` buys: an `Option<Cleared>` would answer `None` either way and
    /// the ordering would be unpinned again.
    #[tokio::test]
    async fn the_holds_are_read_after_the_scan_not_before_it() {
        let store = Store::open_in_memory().await.unwrap();
        let health = GitHubHealth::default();
        let updates = crate::updates::UpdateWatch::at_boot(true);
        let dir = tempfile::tempdir().unwrap();
        let (_svc, client) = pool_with_room(dir.path(), 4).await;
        let pool_health = crate::pool_health::PoolHealth::new();
        let handle = client.handle();

        // Every hold live at once, so no single one of them can be the reason
        // this passes.
        store.set_mode(Mode::Pause).await.unwrap();
        health.observe(&unavailable(), Utc::now());
        crate::images::observe(
            &store,
            "agent:v1",
            tasks_api::version::ImageRole::Scout,
            None,
            "run-1",
        )
        .await;
        pool_health.observe(&full(), Utc::now());
        assert!(
            dispatch_held(&store, &health, &updates, &pool_health, &handle)
                .await
                .unwrap(),
            "the four holds really are in force"
        );

        // ...and the only task is in `Backlog`, which is never dispatchable.
        let project = project();
        store.insert_project(&project).await.unwrap();
        seed_task(&store, &project, 1, TaskState::Backlog).await;

        let skip: HashSet<TaskId> = HashSet::new();
        assert!(
            matches!(
                next_scout(&store, &health, &updates, &pool_health, &handle, &skip)
                    .await
                    .unwrap(),
                NextScout::Drained
            ),
            "the scan has to run first: only it can say the queue is empty, and \
             a hold read ahead of it would answer Held"
        );
    }

    /// The predicate itself, deterministically: every read sees the state as of
    /// that read, never a snapshot taken earlier in the pass (#948).
    ///
    /// Kept deliberately alongside the two above, and it is the narrower
    /// statement: it is what fails first if `dispatch_held` itself breaks
    /// rather than its call site. It is also #973's complaint reproduced —
    /// neither mutation above touches this test, which is exactly why it was
    /// not enough on its own.
    #[tokio::test]
    async fn dispatch_held_answers_from_live_state_every_time() {
        let store = Store::open_in_memory().await.unwrap();
        let health = GitHubHealth::default();
        let updates = crate::updates::UpdateWatch::at_boot(true);
        let dir = tempfile::tempdir().unwrap();
        let (_svc, client) = pool_with_room(dir.path(), 4).await;
        let pool_health = crate::pool_health::PoolHealth::new();
        let handle = client.handle();

        store.set_mode(Mode::Play).await.unwrap();
        assert!(
            !dispatch_held(&store, &health, &updates, &pool_health, &handle)
                .await
                .unwrap(),
            "playing, GitHub answering, nothing pending: dispatch is free"
        );

        for mode in [Mode::Pause, Mode::Stop] {
            store.set_mode(mode).await.unwrap();
            assert!(
                dispatch_held(&store, &health, &updates, &pool_health, &handle)
                    .await
                    .unwrap(),
                "{mode:?} must be seen by the very next read"
            );
            store.set_mode(Mode::Play).await.unwrap();
            assert!(
                !dispatch_held(&store, &health, &updates, &pool_health, &handle)
                    .await
                    .unwrap(),
                "and so must the play that follows it"
            );
        }

        let now = Utc::now();
        health.observe(&unavailable(), now);
        assert!(
            dispatch_held(&store, &health, &updates, &pool_health, &handle)
                .await
                .unwrap(),
            "an outage that started mid-pass holds the next dispatch"
        );
        health.observe(&Ok::<(), GhError>(()), now);
        assert!(
            !dispatch_held(&store, &health, &updates, &pool_health, &handle)
                .await
                .unwrap(),
            "and a success releases it just as promptly"
        );

        // The update hold, which this test never had: an image observed stale
        // under this very watch, and a watch that is no longer enforcing.
        crate::images::observe(
            &store,
            "agent:v1",
            tasks_api::version::ImageRole::Scout,
            None,
            "run-1",
        )
        .await;
        assert!(
            dispatch_held(&store, &health, &updates, &pool_health, &handle)
                .await
                .unwrap(),
            "a stale image observed since boot holds new scouts"
        );
        let off = crate::updates::UpdateWatch::at_boot(false);
        assert!(
            !dispatch_held(&store, &health, &off, &pool_health, &handle)
                .await
                .unwrap(),
            "TASKS_UPDATE_HOLD=off keeps the report and drops the gate"
        );

        // And the pool. The probes above are spent, so this record is what the
        // next read sees.
        pool_health.observe(&full(), Utc::now());
        assert!(
            dispatch_held(&store, &health, &off, &pool_health, &handle)
                .await
                .unwrap(),
            "a full pool holds new scouts (#967)"
        );
        pool_health.observe(&has_room(), Utc::now());
        assert!(
            !dispatch_held(&store, &health, &off, &pool_health, &handle)
                .await
                .unwrap(),
            "and a slot coming back releases it"
        );
    }

    /// A paused repo is a repo the dispatcher walks *past*, not one it stops
    /// at. That `continue` rather than `break` is the whole difference between
    /// pausing one repo and pausing the server.
    #[tokio::test]
    async fn next_dispatchable_skips_a_paused_repo_without_starving_the_queue() {
        let store = Store::open_in_memory().await.unwrap();
        let paused = project();
        store.insert_project(&paused).await.unwrap();
        let live = Project {
            id: ProjectId::new(),
            repo_owner: "iamnbutler".into(),
            repo_name: "other".into(),
            added_at: Utc::now(),
            status: ProjectStatus::Active,
        };
        store.insert_project(&live).await.unwrap();

        // The paused repo's task is at the head of the queue.
        let head = seed_task(&store, &paused, 1, TaskState::Queued).await;
        let behind = seed_task(&store, &live, 2, TaskState::Queued).await;
        store
            .set_queue_order(&[head.id.clone(), behind.id.clone()])
            .await
            .unwrap();
        store
            .set_project_status(&paused.id, ProjectStatus::Paused)
            .await
            .unwrap();

        let skip = HashSet::new();
        let (task, project) = next_dispatchable(&store, &skip)
            .await
            .unwrap()
            .expect("the repo behind the paused one is still dispatchable");
        assert_eq!(task.id, behind.id);
        assert_eq!(project.id, live.id);

        // Pause that one too and there is simply nothing to dispatch — the
        // head's task is still `queued`, not rejected or returned.
        store
            .set_project_status(&live.id, ProjectStatus::Paused)
            .await
            .unwrap();
        assert!(next_dispatchable(&store, &skip).await.unwrap().is_none());
        assert_eq!(
            store.get_task(&head.id).await.unwrap().unwrap().state,
            TaskState::Queued
        );
    }
}
