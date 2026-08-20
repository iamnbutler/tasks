//! Why a pane is empty — one ordered diagnosis, rendered in four places
//! (#992).
//!
//! The app has four blank panes and one sentence between them, so "no
//! server", "no repositories", "400 issues sitting in backlog" and
//! "everything shipped" all render identically. Every fact needed to tell
//! them apart already exists and nothing joins them: the lists, the mode and
//! the connection live in [`crate::state::AppState`], while the three
//! dispatch holds live on `GET /status`, which until now only the Server
//! window ever read.
//!
//! This module is the join and the walk, and it is deliberately **gpui-free**
//! — pure functions over plain data, the [`crate::feed`] / [`crate::chat_log`]
//! precedent. `app-gpui` is not a workspace member, so `make test` compiles
//! none of it; what keeps fourteen states honest is that every one of them is
//! decided here, by a function a unit test can call, rather than by a
//! condition spelled out at a render site nothing can run.
//!
//! ## What this module owns, and what it hands on
//!
//! Three approved specs touch the same surfaces, and the boundaries are
//! written down here so they survive without the spec text:
//!
//! - **#991 (start the server from the app)** owns *whether this app can find
//!   a binary and what to say when it cannot*. This module owns
//!   [`Situation::NoServer`] as a sentence and a button; the button
//!   dispatches the existing `menus::RestartServer` action and claims nothing
//!   about where the binary came from. `crate::server::resolve_binary` never
//!   fails — its last resort is a bare `PathBuf::from("tasks")` — so "no
//!   binary" is not expressible today, and a sentence about it would be a
//!   guess. #991 adds a `NoServerBinary` situation, or a field on
//!   `NoServer`, and a second [`Action`]; both are local to this file.
//! - **#993 (say what `play` will do)** owns *the confirmation between
//!   pressing play and dispatch*. This module owns [`Situation::Paused`] /
//!   [`Situation::Stopped`] and [`Action::Play`], which goes through
//!   `Workspace::set_mode` — the same path the title bar's play button takes,
//!   so a prompt added there is inherited here for free.
//! - **#1005 (paste credentials in)** owns *the token*. This module owns
//!   [`Situation::NoTasks`], whose sentence says the poller needs a token on
//!   the server and deliberately does **not** guess whether one is
//!   configured: nothing in the app can see that today. #1005 adds the
//!   observation, and either a situation above `NoTasks` or an action on it.
//!
//! The caution on the Play button is **not** ours to word: it is
//! [`crate::disclaimer::PLAY_TOOLTIP`] and [`crate::disclaimer::PIPELINE_CAUTION`],
//! read by the render site. This is a fourth surface for a control the other
//! three already caution, and it is the one a new reader meets first — a
//! second wording here would be the disagreement [`Action`] exists as an enum
//! to prevent, one module boundary out.

use tasks_client::api::http::ServerStatus;
use tasks_client::api::models::{
    Mode, Project, ProjectStatus, SpecQueueItem, SpecQueueStatus, Task, TaskState,
};

use crate::projects::ProjectFilter;

/// How much of the server this app can currently see.
///
/// The rail banner's precedence, stated once so three placements cannot
/// disagree with it: `build_warning` outranks `error`, because when this app
/// is under the server's floor whatever failed underneath is the symptom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    /// The stream has not come up yet and nothing has failed. The opening
    /// moments of a launch.
    Connecting,
    /// The stream is down and something said why.
    Down,
    /// The server refused this build as too old.
    Incompatible,
    /// The stream is up.
    Up,
}

impl Reachability {
    /// Read it off the three flags `AppState` keeps.
    ///
    /// `loaded` is deliberately **not** an input: a dropped stream is `Down`
    /// whether or not a snapshot landed earlier, and a placement that wants
    /// "no snapshot yet" asks [`explain_reachability`] instead.
    pub fn read(incompatible: bool, connected: bool, failed: bool) -> Self {
        if incompatible {
            Reachability::Incompatible
        } else if connected {
            Reachability::Up
        } else if failed {
            Reachability::Down
        } else {
            Reachability::Connecting
        }
    }
}

/// Which of the five dispatch holds this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldKind {
    /// An upgrade is half-applied — `make restart` / `make images`.
    Update,
    /// vm-pool has no free slot.
    Pool,
    /// The credential broker is not answering.
    Broker,
    /// This host's container runtime is not running.
    Runtime,
    /// GitHub is not answering.
    GitHub,
}

/// One reason new containers are waiting, as `GET /status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub kind: HoldKind,
    /// Prose for a reader, naming its own discharge.
    pub sentence: String,
    /// Whether this hold is actually gating dispatch.
    ///
    /// `TASKS_UPDATE_HOLD=off` keeps the report and drops the gate, so a
    /// non-binding hold must never be *the reason* nothing is starting — it
    /// is still said, as a note. The other four have no such switch.
    pub binding: bool,
}

/// The five holds, in the order of who can discharge them: the operator's own
/// commands first — `make restart` / `make images`, then `container system
/// start` — then a pool that clears when a VM comes back, then the broker
/// (this server's own listener), and last GitHub, which nobody here can
/// hurry.
pub fn observe(status: Option<&ServerStatus>) -> Vec<Hold> {
    let Some(status) = status else {
        return Vec::new();
    };
    let mut holds = Vec::new();
    if let Some(update) = &status.update {
        // The server's reasons already name `make restart` / `make images`
        // apiece, which is why they are quoted rather than summarised.
        let reasons = if update.reasons.is_empty() {
            "run `make restart`".to_string()
        } else {
            update.reasons.join("; ")
        };
        holds.push(Hold {
            kind: HoldKind::Update,
            sentence: if update.enforced {
                format!("An update is half-applied, so no new run starts: {reasons}.")
            } else {
                format!(
                    "An update is half-applied, and the gate is off \
                     (`TASKS_UPDATE_HOLD`), so runs start anyway: {reasons}."
                )
            },
            binding: update.enforced,
        });
    }
    if let Some(runtime) = &status.runtime {
        holds.push(Hold {
            kind: HoldKind::Runtime,
            sentence: format!(
                "The container runtime is not running ({}), so nothing here can \
                 start a VM at all. Run `container system start`.",
                runtime.error
            ),
            binding: true,
        });
    }
    if let Some(pool) = &status.pool {
        holds.push(Hold {
            kind: HoldKind::Pool,
            sentence: format!(
                "vm-pool has 0 of {} VMs free, so nothing can be dispatched. \
                 It clears when a run hands one back; `VM_POOL_MAX_VMS` is \
                 what sizes the pool.",
                pool.total
            ),
            binding: true,
        });
    }
    if let Some(broker) = &status.broker {
        holds.push(Hold {
            kind: HoldKind::Broker,
            sentence: format!(
                "The credential broker at {} is not answering ({}), and every \
                 clone inside a VM is redeemed there — so work dispatched now \
                 would die at the clone and be charged an attempt for it. It \
                 clears on the first probe that gets a 401.",
                broker.address, broker.error
            ),
            binding: true,
        });
    }
    if let Some(github) = &status.github {
        holds.push(Hold {
            kind: HoldKind::GitHub,
            sentence: format!(
                "GitHub is not answering ({}), and work dispatched into an \
                 outage dies at its first clone. It clears on the first poll \
                 GitHub answers.",
                github.error
            ),
            binding: true,
        });
    }
    holds
}

/// The pipeline counted once, so the walk below reads numbers rather than
/// slices.
///
/// Counts and not lists on purpose: every question the diagnosis asks is
/// "is there any", and a slice invites a placement to answer a different
/// question from the one it renders.
#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub reachability: Reachability,
    /// Repositories that are not archived. An archived repo dispatches
    /// nothing, so counting it would answer "you have repositories" to a
    /// reader whose pipeline can never move.
    pub projects: usize,
    pub tasks: usize,
    pub backlog: usize,
    /// Parked and ready to move: `Queued` + `ReadyToBuild`.
    pub queued: usize,
    /// In a VM right now: `Scouting` + `Building`.
    pub running: usize,
    /// Pending entries in `spec_queue` — **not** tasks in `TaskState::InReview`.
    ///
    /// `rail::band` deliberately excludes `InReview` from the tree, so review
    /// work is exactly the case where the tree is empty and the pipeline is
    /// not idle. The number has to match the rows Awaiting Feedback actually
    /// shows, and those are the queue's, cross-project — which is also why
    /// this one count is not scoped by the repo filter.
    pub awaiting_review: usize,
    pub awaiting_merge: usize,
    pub mode: Option<Mode>,
    pub holds: Vec<Hold>,
}

impl Pipeline {
    /// An empty pipeline at a given reachability — the base every test and
    /// the `!loaded` placements start from.
    pub fn new(reachability: Reachability) -> Self {
        Self {
            reachability,
            projects: 0,
            tasks: 0,
            backlog: 0,
            queued: 0,
            running: 0,
            awaiting_review: 0,
            awaiting_merge: 0,
            mode: None,
            holds: Vec::new(),
        }
    }

    /// Count one window's worth of pipeline.
    ///
    /// `filter` scopes the **tasks** and not the projects: the sentence sits
    /// where the rows would be, so it has to be about the rows that would be
    /// there — but "no repositories" is a global fact, and a filter can only
    /// ever point at a repository that exists.
    pub fn count(
        reachability: Reachability,
        projects: &[Project],
        tasks: &[Task],
        spec_queue: &[SpecQueueItem],
        filter: &ProjectFilter,
        mode: Option<Mode>,
        holds: Vec<Hold>,
    ) -> Self {
        let mut pipeline = Pipeline::new(reachability);
        pipeline.projects = projects
            .iter()
            .filter(|project| project.status != ProjectStatus::Archived)
            .count();
        pipeline.mode = mode;
        pipeline.holds = holds;
        for task in tasks.iter().filter(|task| filter.admits(&task.project_id)) {
            pipeline.tasks += 1;
            match task.state {
                TaskState::Backlog => pipeline.backlog += 1,
                TaskState::Queued | TaskState::ReadyToBuild => pipeline.queued += 1,
                TaskState::Scouting | TaskState::Building => pipeline.running += 1,
                TaskState::AwaitingMerge => pipeline.awaiting_merge += 1,
                TaskState::InReview | TaskState::Done | TaskState::Rejected => {}
            }
        }
        pipeline.awaiting_review = spec_queue
            .iter()
            .filter(|item| item.entry.status == SpecQueueStatus::PendingReview)
            .count();
        pipeline
    }

    /// The first hold that is actually gating dispatch, if any.
    pub fn binding_hold(&self) -> Option<&Hold> {
        self.holds.iter().find(|hold| hold.binding)
    }
}

/// Exactly one thing to say about a pane, chosen by [`diagnose`].
///
/// Fourteen, enumerated from the code rather than from the issue's four. Three
/// of them ([`Situation::Working`], [`Situation::Dispatching`],
/// [`Situation::AwaitingMerge`]) have no placement today and are kept
/// deliberately: `Working` is what stops `Idle` — "nothing owed a decision" —
/// being reported over a running pipeline by any placement added later, and
/// deleting them makes the walk non-total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Situation {
    /// The stream has not come up yet.
    Connecting,
    /// Nothing is serving. See the module comment for the #991 boundary.
    NoServer,
    /// The server refused this build.
    Incompatible,
    /// Nothing is tracked.
    NoProjects,
    /// Repositories are tracked and no issue has arrived.
    NoTasks,
    /// Work is queued and the mode is `stop`.
    Stopped,
    /// Work is queued and the mode is `pause`.
    Paused,
    /// Work is queued, the mode is `play`, and a hold is binding.
    Held,
    /// Work is queued, the mode is `play`, and nothing is in the way.
    Dispatching,
    /// A Scout or a Builder is in a VM.
    Working,
    /// Specs are waiting on a human verdict.
    AwaitingReview,
    /// A batch has shipped a pull request and is parked on it.
    AwaitingMerge,
    /// There is a backlog and nothing has been picked up.
    NothingQueued,
    /// Everything this window can see has shipped.
    Idle,
}

/// What a button in an empty pane does.
///
/// An enum and not a closure, so the placements cannot disagree about what
/// "Play" means — and so every one of them routes through the same action the
/// menus dispatch. There is no second path to starting the server, which is
/// what keeps the Server window's confirmation the only confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StartServer,
    RestartServer,
    AddRepo,
    OpenAllTasks,
    Play,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::StartServer => "Start Server",
            Action::RestartServer => "Restart Server…",
            Action::AddRepo => "Add a Repository",
            Action::OpenAllTasks => "Open All Tasks",
            Action::Play => "Play",
        }
    }

    /// Whether pressing this starts the pipeline dispatching — the one thing
    /// a render site has to know in order to caution it.
    pub fn starts_the_pipeline(self) -> bool {
        matches!(self, Action::Play)
    }
}

/// One headline, one sentence, at most one button, and whatever else is true.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    pub situation: Situation,
    pub headline: &'static str,
    pub detail: String,
    pub action: Option<Action>,
    /// Holds that are true but are not the headline. Said, never pressed.
    pub notes: Vec<String>,
}

impl Explanation {
    /// Whether this describes a rail that *has* rows.
    ///
    /// Exactly the three situations that need something queued and report why
    /// it is not moving: a standing line above a list, rather than a sentence
    /// where the list would have been.
    pub fn is_standing(&self) -> bool {
        matches!(
            self.situation,
            Situation::Paused | Situation::Stopped | Situation::Held
        )
    }

    /// Drop an action that would lead nowhere — "Open All Tasks", rendered in
    /// All Tasks.
    pub fn without(mut self, action: Action) -> Self {
        if self.action == Some(action) {
            self.action = None;
        }
        self
    }
}

/// The ordered walk. Exactly one answer, always.
///
/// **Why this order.** Reachability first, because every list below it is a
/// claim about a snapshot that a dead stream did not deliver. Then the two
/// structural absences — no repositories, then no issues — because neither
/// has anything downstream to be wrong about. Then queued work, which reads
/// the mode, because "3 queued" and "3 queued and nothing will start" are
/// different sentences with different buttons. Then activity, then the two
/// parked states, then the backlog, then success.
///
/// The one step worth defending is **running before awaiting-review**: a
/// reader can act on a spec that wants a verdict and can do nothing at all
/// about a Scout that is running, so ranking activity above it looks
/// inverted. It is not, for a structural reason rather than a taste one —
/// *the two cannot both be candidates in any placement that exists.* Every
/// placement that can report `AwaitingReview` requires an empty list
/// (`rail::tree_rows` empty, or the catalog empty), and `rail::band` puts
/// `Scouting` and `Building` in the tree while the catalog shows every
/// non-done task; a running task is in both, under the same repo filter this
/// walk counts with. So `running > 0` implies the list has a row and the
/// diagnosis is not rendered at that placement at all. There is a second,
/// weaker argument — with `auto_review_specs` live in the charter,
/// awaiting-review is transient and self-clearing, so headlining it would be
/// noise most of the time — but that one is charter-dependent and this one is
/// not, so it is the reason of record. A placement added later that can show
/// both should revisit this line rather than assume it.
pub fn diagnose(pipeline: &Pipeline) -> Situation {
    match pipeline.reachability {
        Reachability::Connecting => return Situation::Connecting,
        Reachability::Down => return Situation::NoServer,
        Reachability::Incompatible => return Situation::Incompatible,
        Reachability::Up => {}
    }
    if pipeline.projects == 0 {
        return Situation::NoProjects;
    }
    if pipeline.tasks == 0 {
        return Situation::NoTasks;
    }
    if pipeline.queued > 0 {
        return match pipeline.mode {
            Some(Mode::Stop) => Situation::Stopped,
            Some(Mode::Play) => match pipeline.binding_hold() {
                Some(_) => Situation::Held,
                None => Situation::Dispatching,
            },
            // An unknown mode is the pre-snapshot value, and queued work is
            // counted from the same snapshot that carries it — so this arm
            // is unreachable today. It reads as paused because "nothing is
            // starting" is the safe reading of a dispatcher nobody can see,
            // and the button it offers is the one that would fix it either
            // way.
            Some(Mode::Pause) | None => Situation::Paused,
        };
    }
    if pipeline.running > 0 {
        return Situation::Working;
    }
    if pipeline.awaiting_review > 0 {
        return Situation::AwaitingReview;
    }
    if pipeline.awaiting_merge > 0 {
        return Situation::AwaitingMerge;
    }
    if pipeline.backlog > 0 {
        return Situation::NothingQueued;
    }
    Situation::Idle
}

/// The walk, plus the words.
///
/// The sentences are written to be read, not templated — they are the
/// deliverable as much as the enum is. Different wording changes one `match`
/// arm here and nothing anywhere else.
pub fn explain(pipeline: &Pipeline) -> Explanation {
    let situation = diagnose(pipeline);
    let (headline, detail, action) = match situation {
        Situation::Connecting => (
            "Connecting to the tasks server…",
            "The event stream is opening. Everything in this window arrives \
             as one snapshot over it."
                .to_string(),
            None,
        ),
        Situation::NoServer => (
            "No server is answering.",
            "Tasks runs as a background service on this machine, and nothing \
             replied. Start it and this window fills itself in."
                .to_string(),
            Some(Action::StartServer),
        ),
        Situation::Incompatible => (
            "This app is older than the server it found.",
            "The server refused this build, so nothing below can be trusted \
             to decode. Restarting the server puts a matching build in front \
             of it."
                .to_string(),
            Some(Action::RestartServer),
        ),
        Situation::NoProjects => (
            "No repositories yet.",
            "Tasks works on GitHub repositories you point it at. Add one and \
             the poller starts filling the backlog from its open issues."
                .to_string(),
            Some(Action::AddRepo),
        ),
        Situation::NoTasks => (
            "No issues have arrived yet.",
            "The poller ingests open GitHub issues on a timer, and it needs a \
             token on the server to do it. Nothing here is queued by hand."
                .to_string(),
            None,
        ),
        Situation::Stopped => (
            "The pipeline is stopped.",
            format!(
                "{} and nothing will start — stop is deliberate, and it \
                 outlives a restart. Press play when you want work moving.",
                queued_phrase(pipeline.queued)
            ),
            Some(Action::Play),
        ),
        Situation::Paused => (
            "The pipeline is paused.",
            format!(
                "{}, and nothing new starts while it is paused. Every boot \
                 comes up this way.",
                queued_phrase(pipeline.queued)
            ),
            Some(Action::Play),
        ),
        Situation::Held => (
            "Dispatch is held.",
            format!(
                "{}, the pipeline is playing, and something is in the way. {}",
                queued_phrase(pipeline.queued),
                pipeline
                    .binding_hold()
                    .map(|hold| hold.sentence.clone())
                    .unwrap_or_default()
            ),
            None,
        ),
        Situation::Dispatching => (
            "Work is queued and moving.",
            format!(
                "{}. The next Scout starts as soon as a VM is free.",
                queued_phrase(pipeline.queued)
            ),
            None,
        ),
        Situation::Working => (
            "Work is running.",
            format!(
                "{} in a VM right now — a Scout exploring, or a Builder \
                 implementing. Nothing is owed a decision.",
                count_phrase(pipeline.running, "run", "runs")
            ),
            None,
        ),
        Situation::AwaitingReview => (
            "Specs are waiting on your review.",
            format!(
                "{} written and none of them approved or sent back. They are \
                 in Awaiting Feedback, at the foot of the rail, across every \
                 repository.",
                count_phrase(pipeline.awaiting_review, "spec", "specs")
            ),
            None,
        ),
        Situation::AwaitingMerge => (
            "Pull requests are open.",
            format!(
                "{} parked on a pull request. Nothing else starts on them \
                 until it reaches the trunk, and merging is what closes the \
                 issue.",
                count_phrase(pipeline.awaiting_merge, "task is", "tasks are")
            ),
            None,
        ),
        Situation::NothingQueued => (
            "Nothing is picked up.",
            format!(
                "{} in the backlog and none of it queued. Bulk intake never \
                 dispatches by itself — queue something from All Tasks and a \
                 Scout picks it up.",
                count_phrase(pipeline.backlog, "issue", "issues")
            ),
            Some(Action::OpenAllTasks),
        ),
        Situation::Idle => (
            "Everything here has shipped.",
            "Nothing queued, nothing running, nothing owed a decision — every \
             issue this window can see is closed."
                .to_string(),
            None,
        ),
    };
    // Whatever the headline already said is not repeated underneath it.
    let headlined = matches!(situation, Situation::Held)
        .then(|| pipeline.binding_hold())
        .flatten();
    let notes = pipeline
        .holds
        .iter()
        .filter(|hold| Some(*hold) != headlined)
        .map(|hold| hold.sentence.clone())
        .collect();
    Explanation {
        situation,
        headline,
        detail,
        action,
        notes,
    }
}

/// The connection half of the walk on its own.
///
/// For the two placements that can only ever report it: the middle column
/// before the first snapshot, and the chat with nothing to talk to. `Up`
/// folds to [`Situation::Connecting`] there rather than walking the lists,
/// because a caller in this state has no snapshot — the stream is open and
/// the first one is still in flight, and reporting "no repositories" off
/// lists that have never been filled would be the loudest possible wrong
/// answer.
pub fn explain_reachability(reachability: Reachability) -> Explanation {
    let mut pipeline = Pipeline::new(reachability);
    if reachability == Reachability::Up {
        pipeline.reachability = Reachability::Connecting;
    }
    explain(&pipeline)
}

fn queued_phrase(queued: usize) -> String {
    format!("{} queued", count_phrase(queued, "task is", "tasks are"))
}

fn count_phrase(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        _ => format!("{n} {many}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tasks_client::api::models::{GhState, ProjectId, SpecId, SpecQueueEntry, TaskId};

    fn project(name: &str, status: ProjectStatus) -> Project {
        Project {
            id: ProjectId::from_raw(name),
            repo_owner: "acme".into(),
            repo_name: name.into(),
            added_at: Utc::now(),
            status,
        }
    }

    /// `n` tasks in one state, with distinct ids.
    fn tasks(project: &str, state: TaskState, n: usize) -> Vec<Task> {
        (0..n)
            .map(|i| Task {
                id: TaskId::from_raw(format!("task-{project}-{}-{i}", state.as_str())),
                project_id: ProjectId::from_raw(project),
                gh_issue_number: i as u64 + 1,
                title: "A task".into(),
                body: String::new(),
                labels: Vec::new(),
                gh_state: GhState::Open,
                state,
                priority: 0,
                manual_rank: None,
                dispatch_attempts: 0,
                scout_directions: None,
                ingested_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .collect()
    }

    fn queue_entry(status: SpecQueueStatus, n: usize) -> SpecQueueItem {
        SpecQueueItem {
            entry: SpecQueueEntry {
                spec_id: SpecId::from_raw(format!("spec-{n}")),
                status,
                rank: None,
                approved_at: None,
                feedback: None,
                blocking_dependencies: Vec::new(),
            },
            task_id: TaskId::from_raw(format!("task-{n}")),
        }
    }

    /// A pipeline that is up, tracked and has work parked in `Queued`.
    fn parked(mode: Mode) -> Pipeline {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        pipeline.tasks = 3;
        pipeline.queued = 3;
        pipeline.mode = Some(mode);
        pipeline
    }

    fn hold(kind: HoldKind, binding: bool) -> Hold {
        Hold {
            kind,
            sentence: match kind {
                HoldKind::Update => "An update is half-applied: run `make restart`.".into(),
                HoldKind::Pool => "vm-pool has 0 of 6 VMs free; `VM_POOL_MAX_VMS` sizes it.".into(),
                HoldKind::Broker => {
                    "the credential broker is not answering; it clears on the next 401.".into()
                }
                HoldKind::Runtime => {
                    "the container runtime is not running: run `container system start`.".into()
                }
                HoldKind::GitHub => "GitHub is not answering; it clears on the next poll.".into(),
            },
            binding,
        }
    }

    // --- reachability ---

    #[test]
    fn reachability_reads_the_rail_banners_precedence() {
        assert_eq!(
            Reachability::read(true, true, true),
            Reachability::Incompatible
        );
        assert_eq!(Reachability::read(false, true, true), Reachability::Up);
        assert_eq!(Reachability::read(false, false, true), Reachability::Down);
        assert_eq!(
            Reachability::read(false, false, false),
            Reachability::Connecting
        );
    }

    /// The whole point of the ordering: a stream that is down says so, and
    /// never reports the stale lists behind it as facts about the pipeline.
    #[test]
    fn reachability_outranks_every_list() {
        for reachability in [
            Reachability::Connecting,
            Reachability::Down,
            Reachability::Incompatible,
        ] {
            let mut pipeline = Pipeline::new(reachability);
            pipeline.projects = 4;
            pipeline.tasks = 400;
            pipeline.backlog = 397;
            pipeline.queued = 3;
            pipeline.mode = Some(Mode::Pause);
            let situation = diagnose(&pipeline);
            assert!(
                matches!(
                    situation,
                    Situation::Connecting | Situation::NoServer | Situation::Incompatible
                ),
                "{reachability:?} reported {situation:?}"
            );
        }
    }

    /// A dropped stream is `Down` whether or not a snapshot landed earlier —
    /// which is why `loaded` is not one of the three flags.
    #[test]
    fn a_dropped_stream_is_down_even_after_a_snapshot() {
        assert_eq!(Reachability::read(false, false, true), Reachability::Down);
        assert_eq!(
            diagnose(&Pipeline::new(Reachability::Down)),
            Situation::NoServer
        );
    }

    #[test]
    fn explain_reachability_never_walks_the_lists() {
        assert_eq!(
            explain_reachability(Reachability::Up).situation,
            Situation::Connecting
        );
        assert_eq!(
            explain_reachability(Reachability::Down).situation,
            Situation::NoServer
        );
        assert_eq!(
            explain_reachability(Reachability::Incompatible).situation,
            Situation::Incompatible
        );
    }

    // --- the walk ---

    #[test]
    fn no_repositories_outranks_no_tasks() {
        let pipeline = Pipeline::new(Reachability::Up);
        assert_eq!(diagnose(&pipeline), Situation::NoProjects);
        assert_eq!(explain(&pipeline).action, Some(Action::AddRepo));
    }

    #[test]
    fn tracked_but_empty_is_not_the_same_as_untracked() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        assert_eq!(diagnose(&pipeline), Situation::NoTasks);
    }

    /// The sentence must not guess at a token it cannot see — that is #1005's,
    /// and a wrong guess here sends the reader to the wrong place.
    #[test]
    fn no_tasks_names_the_poller_without_claiming_a_token_is_missing() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        let explanation = explain(&pipeline);
        assert!(explanation.detail.contains("token"));
        assert!(!explanation.detail.to_lowercase().contains("no token"));
        assert_eq!(explanation.action, None);
    }

    #[test]
    fn queued_work_reads_the_mode() {
        assert_eq!(diagnose(&parked(Mode::Pause)), Situation::Paused);
        assert_eq!(diagnose(&parked(Mode::Stop)), Situation::Stopped);
        assert_eq!(diagnose(&parked(Mode::Play)), Situation::Dispatching);
    }

    /// The failure this whole module exists to end: a paused queue reading as
    /// an idle pipeline.
    #[test]
    fn paused_with_queued_work_is_not_idle() {
        let explanation = explain(&parked(Mode::Pause));
        assert_eq!(explanation.situation, Situation::Paused);
        assert_eq!(explanation.action, Some(Action::Play));
        assert!(explanation.detail.contains("3 tasks are queued"));
    }

    /// A running scout does not make a paused queue look like a moving one:
    /// the mode branch is taken first, and it is the reason nothing else
    /// starts.
    #[test]
    fn a_running_scout_does_not_hide_a_paused_queue() {
        let mut pipeline = parked(Mode::Pause);
        pipeline.running = 2;
        pipeline.tasks += 2;
        assert_eq!(diagnose(&pipeline), Situation::Paused);
    }

    #[test]
    fn an_unknown_mode_reads_as_paused() {
        let mut pipeline = parked(Mode::Pause);
        pipeline.mode = None;
        assert_eq!(diagnose(&pipeline), Situation::Paused);
    }

    #[test]
    fn running_review_merge_and_backlog_are_walked_in_order() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        pipeline.tasks = 4;
        pipeline.mode = Some(Mode::Play);
        pipeline.running = 1;
        pipeline.awaiting_review = 1;
        pipeline.awaiting_merge = 1;
        pipeline.backlog = 1;
        assert_eq!(diagnose(&pipeline), Situation::Working);
        pipeline.running = 0;
        assert_eq!(diagnose(&pipeline), Situation::AwaitingReview);
        pipeline.awaiting_review = 0;
        assert_eq!(diagnose(&pipeline), Situation::AwaitingMerge);
        pipeline.awaiting_merge = 0;
        assert_eq!(diagnose(&pipeline), Situation::NothingQueued);
        pipeline.backlog = 0;
        assert_eq!(diagnose(&pipeline), Situation::Idle);
    }

    /// The one placement `AwaitingReview` exists for: `rail::band` keeps
    /// `InReview` out of the tree, so a pipeline with nothing but review work
    /// shows an empty tree and is emphatically not idle.
    #[test]
    fn review_work_alone_is_not_idle() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        pipeline.tasks = 3;
        pipeline.mode = Some(Mode::Play);
        pipeline.awaiting_review = 3;
        assert_eq!(diagnose(&pipeline), Situation::AwaitingReview);
        assert!(explain(&pipeline).detail.contains("3 specs"));
    }

    #[test]
    fn everything_landed_is_success_and_says_so() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 2;
        pipeline.tasks = 40;
        pipeline.mode = Some(Mode::Play);
        let explanation = explain(&pipeline);
        assert_eq!(explanation.situation, Situation::Idle);
        assert!(explanation.headline.contains("shipped"));
        assert_eq!(explanation.action, None);
        assert!(explanation.notes.is_empty());
    }

    // --- holds ---

    #[test]
    fn a_binding_hold_under_a_playing_queue_is_the_headline() {
        let mut pipeline = parked(Mode::Play);
        pipeline.holds = vec![hold(HoldKind::Pool, true)];
        let explanation = explain(&pipeline);
        assert_eq!(explanation.situation, Situation::Held);
        assert!(explanation.detail.contains("vm-pool"));
        // The headline said it; it is not repeated as a note.
        assert!(explanation.notes.is_empty());
        assert_eq!(explanation.action, None);
    }

    /// `TASKS_UPDATE_HOLD=off` keeps the report and drops the gate, so the
    /// pipeline really is dispatching — telling the reader to wait for
    /// something that is not going to happen is the failure.
    #[test]
    fn a_non_binding_hold_never_becomes_the_reason() {
        let mut pipeline = parked(Mode::Play);
        pipeline.holds = vec![hold(HoldKind::Update, false)];
        let explanation = explain(&pipeline);
        assert_eq!(explanation.situation, Situation::Dispatching);
        assert_eq!(explanation.notes.len(), 1);
    }

    /// A hold is true under a paused pipeline too — but the mode is why
    /// nothing is starting, and pressing play is what the reader can do.
    #[test]
    fn a_hold_under_a_paused_pipeline_is_a_note_not_the_headline() {
        let mut pipeline = parked(Mode::Pause);
        pipeline.holds = vec![hold(HoldKind::GitHub, true)];
        let explanation = explain(&pipeline);
        assert_eq!(explanation.situation, Situation::Paused);
        assert_eq!(explanation.action, Some(Action::Play));
        assert_eq!(explanation.notes.len(), 1);
        assert!(explanation.notes[0].contains("GitHub"));
    }

    #[test]
    fn the_other_holds_survive_as_notes_beside_the_headline() {
        let mut pipeline = parked(Mode::Play);
        pipeline.holds = vec![
            hold(HoldKind::Update, true),
            hold(HoldKind::Pool, true),
            hold(HoldKind::GitHub, true),
        ];
        let explanation = explain(&pipeline);
        assert_eq!(explanation.situation, Situation::Held);
        assert!(explanation.detail.contains("make restart"));
        assert_eq!(explanation.notes.len(), 2);
    }

    /// The reader's next question is always "what do I run" — every hold
    /// sentence has to answer it, including the ones nobody can hurry.
    #[test]
    fn every_hold_sentence_names_its_own_discharge() {
        let status = status_with_every_hold(true);
        let holds = observe(Some(&status));
        assert_eq!(holds.len(), 5);
        assert!(holds[0].sentence.contains("make restart"));
        assert!(holds[1].sentence.contains("container system start"));
        assert!(holds[2].sentence.contains("VM_POOL_MAX_VMS"));
        assert!(holds[3].sentence.contains("clears on the first probe"));
        assert!(holds[4].sentence.contains("clears on the first poll"));
        // The broker hold names its address, because that is the thing the
        // reader checks — and because it is deliberately the advertised one
        // and not loopback (#1006).
        assert!(holds[3].sentence.contains("192.168.64.1:4801"));
        // And the runtime hold quotes what `container system status` said: a
        // stopped service and a broken install read identically once
        // summarised (#1017).
        assert!(holds[1].sentence.contains("not registered with launchd"));
    }

    #[test]
    fn holds_are_ordered_by_who_can_discharge_them() {
        let status = status_with_every_hold(true);
        let kinds: Vec<_> = observe(Some(&status))
            .into_iter()
            .map(|hold| hold.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                HoldKind::Update,
                HoldKind::Runtime,
                HoldKind::Pool,
                HoldKind::Broker,
                HoldKind::GitHub
            ]
        );
    }

    #[test]
    fn an_unenforced_update_hold_is_observed_as_non_binding() {
        let status = status_with_every_hold(false);
        let holds = observe(Some(&status));
        assert!(!holds[0].binding);
        assert!(holds[0].sentence.contains("TASKS_UPDATE_HOLD"));
        assert!(holds[1..].iter().all(|hold| hold.binding));
    }

    /// Nothing observed is not a clean bill of health — but it is also not a
    /// hold, and a `None` status is what a server nobody has polled looks
    /// like.
    #[test]
    fn no_status_observes_no_holds() {
        assert!(observe(None).is_empty());
    }

    fn status_with_every_hold(enforced: bool) -> ServerStatus {
        use tasks_client::api::http::{
            BrokerHold, GitHubHold, InFlight, PoolHold, RuntimeHold, UpdatePending,
        };
        ServerStatus {
            pid: 1,
            started_at: Utc::now(),
            migrations_applied: Vec::new(),
            mode: Mode::Play,
            in_flight: InFlight::default(),
            images: Vec::new(),
            github: Some(GitHubHold {
                since: Utc::now(),
                last_seen: Utc::now(),
                failures: 3,
                error: "502 Bad Gateway".into(),
            }),
            update: Some(UpdatePending {
                reasons: vec!["a newer server binary is on disk; run `make restart`".into()],
                enforced,
            }),
            pool: Some(PoolHold {
                since: Utc::now(),
                last_seen: Utc::now(),
                observations: 4,
                total: 6,
            }),
            runtime: Some(RuntimeHold {
                since: Utc::now(),
                last_seen: Utc::now(),
                probes: 2,
                error: "apiserver is not running and not registered with launchd".into(),
            }),
            broker: Some(BrokerHold {
                since: Utc::now(),
                last_seen: Utc::now(),
                probes: 2,
                address: "192.168.64.1:4801".into(),
                error: "it accepted the connection and then it returned no bytes at all".into(),
            }),
        }
    }

    // --- counting ---

    /// An archived repository dispatches nothing, so counting it would answer
    /// "you have repositories" to a reader whose pipeline can never move.
    #[test]
    fn archived_repositories_do_not_count_as_repositories() {
        let projects = vec![
            project("gone", ProjectStatus::Archived),
            project("also-gone", ProjectStatus::Archived),
        ];
        let pipeline = Pipeline::count(
            Reachability::Up,
            &projects,
            &[],
            &[],
            &ProjectFilter::All,
            Some(Mode::Play),
            Vec::new(),
        );
        assert_eq!(pipeline.projects, 0);
        assert_eq!(diagnose(&pipeline), Situation::NoProjects);
    }

    /// Paused is not archived: a paused repo is one you can un-pause, and its
    /// rows are on screen.
    #[test]
    fn a_paused_repository_still_counts() {
        let projects = vec![project("held", ProjectStatus::Paused)];
        let pipeline = Pipeline::count(
            Reachability::Up,
            &projects,
            &[],
            &[],
            &ProjectFilter::All,
            Some(Mode::Play),
            Vec::new(),
        );
        assert_eq!(pipeline.projects, 1);
        assert_eq!(diagnose(&pipeline), Situation::NoTasks);
    }

    #[test]
    fn tasks_are_counted_into_the_bands_the_walk_asks_about() {
        let mut all = tasks("repo", TaskState::Backlog, 4);
        all.extend(tasks("repo", TaskState::Queued, 2));
        all.extend(tasks("repo", TaskState::ReadyToBuild, 1));
        all.extend(tasks("repo", TaskState::Scouting, 1));
        all.extend(tasks("repo", TaskState::Building, 1));
        all.extend(tasks("repo", TaskState::AwaitingMerge, 3));
        all.extend(tasks("repo", TaskState::InReview, 2));
        all.extend(tasks("repo", TaskState::Done, 9));
        let pipeline = Pipeline::count(
            Reachability::Up,
            &[project("repo", ProjectStatus::Active)],
            &all,
            &[],
            &ProjectFilter::All,
            Some(Mode::Play),
            Vec::new(),
        );
        assert_eq!(pipeline.tasks, 23);
        assert_eq!(pipeline.backlog, 4);
        assert_eq!(pipeline.queued, 3);
        assert_eq!(pipeline.running, 2);
        assert_eq!(pipeline.awaiting_merge, 3);
    }

    /// The sentence sits where the rows would be, so it has to be about the
    /// rows that would be there. This is also what keeps the rail's two
    /// placements mutually exclusive — see `render_explanation`'s callers.
    #[test]
    fn tasks_are_scoped_by_the_repo_filter_and_projects_are_not() {
        let projects = vec![
            project("here", ProjectStatus::Active),
            project("elsewhere", ProjectStatus::Active),
        ];
        let mut all = tasks("here", TaskState::Backlog, 2);
        all.extend(tasks("elsewhere", TaskState::Queued, 5));
        let scoped = Pipeline::count(
            Reachability::Up,
            &projects,
            &all,
            &[],
            &ProjectFilter::One(ProjectId::from_raw("here")),
            Some(Mode::Pause),
            Vec::new(),
        );
        assert_eq!(scoped.projects, 2);
        assert_eq!(scoped.tasks, 2);
        assert_eq!(scoped.queued, 0);
        assert_eq!(diagnose(&scoped), Situation::NothingQueued);
    }

    /// From `spec_queue`, so the number matches the rows Awaiting Feedback
    /// shows — which are the queue's pending entries, and cross-project.
    #[test]
    fn awaiting_review_counts_the_queue_and_not_the_task_state() {
        let in_review = tasks("repo", TaskState::InReview, 5);
        let queue = vec![
            queue_entry(SpecQueueStatus::PendingReview, 1),
            queue_entry(SpecQueueStatus::PendingReview, 2),
        ];
        let pipeline = Pipeline::count(
            Reachability::Up,
            &[project("repo", ProjectStatus::Active)],
            &in_review,
            &queue,
            &ProjectFilter::All,
            Some(Mode::Play),
            Vec::new(),
        );
        assert_eq!(pipeline.awaiting_review, 2);
        assert_eq!(diagnose(&pipeline), Situation::AwaitingReview);
    }

    #[test]
    fn a_settled_queue_entry_is_not_awaiting_review() {
        let settled = queue_entry(SpecQueueStatus::Approved, 1);
        let pipeline = Pipeline::count(
            Reachability::Up,
            &[project("repo", ProjectStatus::Active)],
            &tasks("repo", TaskState::Done, 1),
            &[settled],
            &ProjectFilter::All,
            Some(Mode::Play),
            Vec::new(),
        );
        assert_eq!(pipeline.awaiting_review, 0);
        assert_eq!(diagnose(&pipeline), Situation::Idle);
    }

    // --- the words and the buttons ---

    /// Fourteen states, fourteen distinct sentences, none of them a stub.
    #[test]
    fn every_situation_has_a_sentence_and_no_placeholder() {
        let mut seen: Vec<(Situation, String, String)> = Vec::new();
        for pipeline in every_situation() {
            let explanation = explain(&pipeline);
            assert!(!explanation.headline.trim().is_empty());
            assert!(!explanation.detail.trim().is_empty());
            for text in [explanation.headline, explanation.detail.as_str()] {
                assert!(!text.contains("TODO"), "{text}");
                assert!(!text.contains('{'), "{text}");
            }
            assert!(
                !seen.iter().any(|(s, _, _)| *s == explanation.situation),
                "{:?} produced twice",
                explanation.situation
            );
            seen.push((
                explanation.situation,
                explanation.headline.to_string(),
                explanation.detail.clone(),
            ));
        }
        assert_eq!(seen.len(), 14);
        for (i, (_, headline, detail)) in seen.iter().enumerate() {
            for (j, (_, other_headline, other_detail)) in seen.iter().enumerate() {
                if i != j {
                    assert!(
                        headline != other_headline || detail != other_detail,
                        "two situations say the same thing: {headline} / {detail}"
                    );
                }
            }
        }
    }

    /// Five actions across six situations — `Paused` and `Stopped` share
    /// `Play`, because they are the same question for the reader ("why is my
    /// queued work not moving?") and the distinction between them is one the
    /// reader did not make and cannot see from that pane.
    #[test]
    fn only_the_situations_with_something_to_press_carry_a_button() {
        let with_buttons: Vec<_> = every_situation()
            .into_iter()
            .map(|pipeline| explain(&pipeline))
            .filter(|explanation| explanation.action.is_some())
            .map(|explanation| (explanation.situation, explanation.action.unwrap()))
            .collect();
        assert_eq!(
            with_buttons,
            vec![
                (Situation::NoServer, Action::StartServer),
                (Situation::Incompatible, Action::RestartServer),
                (Situation::NoProjects, Action::AddRepo),
                (Situation::Stopped, Action::Play),
                (Situation::Paused, Action::Play),
                (Situation::NothingQueued, Action::OpenAllTasks),
            ]
        );
    }

    /// Exactly the three that describe a rail with rows in it. Everything
    /// else replaces a list rather than standing above one.
    #[test]
    fn only_the_parked_modes_are_standing() {
        let standing: Vec<_> = every_situation()
            .into_iter()
            .map(|pipeline| explain(&pipeline))
            .filter(Explanation::is_standing)
            .map(|explanation| explanation.situation)
            .collect();
        assert_eq!(
            standing,
            vec![Situation::Stopped, Situation::Paused, Situation::Held]
        );
    }

    /// Every standing situation needs something queued — which is what makes
    /// the rail's two placements mutually exclusive under one repo filter: an
    /// empty tree means nothing queued, and a standing line means there is.
    #[test]
    fn a_standing_situation_always_has_queued_work() {
        for pipeline in every_situation() {
            if explain(&pipeline).is_standing() {
                assert!(pipeline.queued > 0, "{pipeline:?}");
            }
        }
    }

    #[test]
    fn without_drops_only_the_action_it_names() {
        let mut pipeline = Pipeline::new(Reachability::Up);
        pipeline.projects = 1;
        pipeline.tasks = 5;
        pipeline.backlog = 5;
        pipeline.mode = Some(Mode::Play);
        let explanation = explain(&pipeline);
        assert_eq!(explanation.action, Some(Action::OpenAllTasks));
        assert_eq!(
            explanation.clone().without(Action::Play).action,
            Some(Action::OpenAllTasks)
        );
        assert_eq!(explanation.without(Action::OpenAllTasks).action, None);
    }

    #[test]
    fn every_action_has_a_label_and_only_play_starts_the_pipeline() {
        for action in [
            Action::StartServer,
            Action::RestartServer,
            Action::AddRepo,
            Action::OpenAllTasks,
            Action::Play,
        ] {
            assert!(!action.label().trim().is_empty());
            assert_eq!(action.starts_the_pipeline(), action == Action::Play);
        }
    }

    #[test]
    fn counts_are_written_for_a_reader_and_not_templated() {
        assert_eq!(count_phrase(1, "spec", "specs"), "1 spec");
        assert_eq!(count_phrase(0, "spec", "specs"), "0 specs");
        assert_eq!(count_phrase(2, "spec", "specs"), "2 specs");
        assert_eq!(queued_phrase(1), "1 task is queued");
        assert_eq!(queued_phrase(4), "4 tasks are queued");
    }

    /// One pipeline per situation, in walk order — the fixture the three
    /// enumeration tests above share, and the thing that goes red when the
    /// walk stops being total.
    fn every_situation() -> Vec<Pipeline> {
        let up = |f: fn(&mut Pipeline)| {
            let mut pipeline = Pipeline::new(Reachability::Up);
            pipeline.projects = 1;
            pipeline.mode = Some(Mode::Play);
            f(&mut pipeline);
            pipeline
        };
        vec![
            Pipeline::new(Reachability::Connecting),
            Pipeline::new(Reachability::Down),
            Pipeline::new(Reachability::Incompatible),
            {
                let mut pipeline = Pipeline::new(Reachability::Up);
                pipeline.mode = Some(Mode::Play);
                pipeline
            },
            up(|p| p.tasks = 0),
            {
                let mut pipeline = parked(Mode::Stop);
                pipeline.holds.push(Hold {
                    kind: HoldKind::Update,
                    sentence: "noted".into(),
                    binding: false,
                });
                pipeline
            },
            parked(Mode::Pause),
            {
                let mut pipeline = parked(Mode::Play);
                pipeline.holds = vec![hold(HoldKind::Pool, true)];
                pipeline
            },
            parked(Mode::Play),
            up(|p| {
                p.tasks = 2;
                p.running = 2;
            }),
            up(|p| {
                p.tasks = 2;
                p.awaiting_review = 2;
            }),
            up(|p| {
                p.tasks = 2;
                p.awaiting_merge = 2;
            }),
            up(|p| {
                p.tasks = 7;
                p.backlog = 7;
            }),
            up(|p| p.tasks = 12),
        ]
    }
}
