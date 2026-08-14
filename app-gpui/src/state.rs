//! App-wide server state, following the Swift app's `AppModel` and the
//! clients.md contract: one event-stream loop drives everything, events are
//! invalidation signals, and every refresh is a full snapshot of the lists.
//!
//! Threading: `tasks-client` is blocking by design. The SSE iterator lives on
//! its own OS thread and feeds an unbounded channel; a foreground task drains
//! the channel into entity updates. Snapshot fetches and mutations run on
//! gpui's background executor (loopback calls are sub-millisecond).

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::Context;
use tasks_client::api::events::Event;
use tasks_client::api::http::BriefingStatus;
use tasks_client::api::models::{
    Build, Mode, OrchestratorMessage, Project, Session, Spec, SpecId, SpecQueueItem,
    SpecQueueStatus, Task, TaskId,
};
use tasks_client::{Client, ClientError, EventStreamItem};

/// Activity keeps the newest slice, refetched per event — the client never
/// holds the whole log (the Swift app did, to reconstruct joins the API now
/// serves directly).
const ACTIVITY_LIMIT: i64 = 200;

pub struct AppState {
    client: Client,

    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub sessions: Vec<Session>,
    pub specs: Vec<Spec>,
    pub spec_queue: Vec<SpecQueueItem>,
    pub builds: Vec<Build>,
    /// Newest [`ACTIVITY_LIMIT`] events, newest first.
    pub activity: Vec<Event>,
    pub briefings: Vec<BriefingStatus>,
    pub orchestrator_messages: Vec<OrchestratorMessage>,
    pub mode: Option<Mode>,

    /// The event stream is up. `false` after a drop, until reconnect.
    pub connected: bool,
    /// First snapshot applied — before this the UI shows "connecting".
    pub loaded: bool,
    /// Last transport/API failure worth a banner. Cleared on reconnect and
    /// on any fully-successful refresh.
    pub error: Option<String>,

    refreshing: bool,
    /// An event arrived while a refresh was in flight — go again after.
    dirty: bool,
}

/// One full read of every list the UI shows. Fields are per-endpoint options
/// so one failing read doesn't blank the others (the Swift app's policy).
#[derive(Default)]
struct Snapshot {
    projects: Option<Vec<Project>>,
    tasks: Option<Vec<Task>>,
    sessions: Option<Vec<Session>>,
    specs: Option<Vec<Spec>>,
    spec_queue: Option<Vec<SpecQueueItem>>,
    builds: Option<Vec<Build>>,
    activity: Option<Vec<Event>>,
    briefings: Option<Vec<BriefingStatus>>,
    orchestrator_messages: Option<Vec<OrchestratorMessage>>,
    mode: Option<Mode>,
    /// The first failure, if any read failed.
    error: Option<String>,
}

impl Snapshot {
    fn fetch(client: &Client) -> Self {
        let mut snapshot = Self::default();
        let mut error: Option<String> = None;
        fn take<T>(
            slot: &mut Option<T>,
            error: &mut Option<String>,
            result: Result<T, ClientError>,
        ) {
            match result {
                Ok(value) => *slot = Some(value),
                Err(err) => {
                    if error.is_none() {
                        *error = Some(err.to_string());
                    }
                }
            }
        }
        take(&mut snapshot.projects, &mut error, client.projects());
        take(&mut snapshot.tasks, &mut error, client.tasks());
        take(&mut snapshot.sessions, &mut error, client.sessions());
        take(&mut snapshot.specs, &mut error, client.specs());
        take(&mut snapshot.spec_queue, &mut error, client.spec_queue());
        take(&mut snapshot.builds, &mut error, client.builds());
        take(
            &mut snapshot.activity,
            &mut error,
            client.events(None, Some(ACTIVITY_LIMIT)).map(|mut events| {
                events.reverse(); // newest first for the feed
                events
            }),
        );
        take(&mut snapshot.briefings, &mut error, client.briefings());
        take(
            &mut snapshot.orchestrator_messages,
            &mut error,
            client.orchestrator_messages(0),
        );
        take(&mut snapshot.mode, &mut error, client.mode());
        snapshot.error = error;
        snapshot
    }
}

impl AppState {
    /// Creates the state and starts the sync loop: a dedicated OS thread runs
    /// the reconnecting SSE iterator, and every `Connected` triggers a full
    /// snapshot — so nothing that happened while disconnected is missed.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let client = Client::from_env();

        let (tx, mut rx) = mpsc::unbounded();
        {
            let client = client.clone();
            std::thread::Builder::new()
                .name("tasks-event-stream".into())
                .spawn(move || {
                    for item in client.stream_events() {
                        if tx.unbounded_send(item).is_err() {
                            return; // app side gone
                        }
                    }
                })
                .expect("spawn event-stream thread");
        }
        cx.spawn(async move |this, cx| {
            while let Some(item) = rx.next().await {
                let alive = this
                    .update(cx, |state: &mut AppState, cx| {
                        state.on_stream_item(item, cx)
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();

        Self {
            client,
            projects: Vec::new(),
            tasks: Vec::new(),
            sessions: Vec::new(),
            specs: Vec::new(),
            spec_queue: Vec::new(),
            builds: Vec::new(),
            activity: Vec::new(),
            briefings: Vec::new(),
            orchestrator_messages: Vec::new(),
            mode: None,
            connected: false,
            loaded: false,
            error: None,
            refreshing: false,
            dirty: false,
        }
    }

    fn on_stream_item(&mut self, item: EventStreamItem, cx: &mut Context<Self>) {
        match item {
            EventStreamItem::Connected => {
                self.connected = true;
                self.error = None;
                self.refresh(cx);
            }
            // Identifier-only invalidation signal: never fold the payload
            // into state, just refetch the lists.
            EventStreamItem::Event(_) => self.refresh(cx),
            EventStreamItem::Disconnected(err) => {
                self.connected = false;
                self.error = Some(err.to_string());
                cx.notify();
            }
        }
    }

    /// Full snapshot on the background executor, applied on completion.
    /// Coalesced: a refresh requested mid-flight runs once more at the end
    /// instead of stacking.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            self.dirty = true;
            return;
        }
        self.refreshing = true;
        let client = self.client.clone();
        let fetch = cx
            .background_executor()
            .spawn(async move { Snapshot::fetch(&client) });
        cx.spawn(async move |this, cx| {
            let snapshot = fetch.await;
            this.update(cx, |state, cx| state.apply(snapshot, cx)).ok();
        })
        .detach();
    }

    fn apply(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        macro_rules! merge {
            ($($field:ident),+) => {
                $(if let Some(value) = snapshot.$field { self.$field = value; })+
            };
        }
        merge!(
            projects,
            tasks,
            sessions,
            specs,
            spec_queue,
            builds,
            activity,
            briefings,
            orchestrator_messages
        );
        if let Some(mode) = snapshot.mode {
            self.mode = Some(mode);
        }
        self.loaded = true;
        self.error = snapshot.error;
        self.refreshing = false;
        if self.dirty {
            self.dirty = false;
            self.refresh(cx);
        }
        cx.notify();
    }

    /// Run a mutation on the background executor; refresh on success, banner
    /// the server's message on failure. The refresh is what applies the
    /// change — responses aren't folded in piecemeal.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Client) -> Result<T, ClientError> + Send + 'static,
    ) {
        let client = self.client.clone();
        let work = cx.background_executor().spawn(async move { op(&client) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |state, cx| match result {
                Ok(_) => state.refresh(cx),
                Err(err) => {
                    state.error = Some(err.to_string());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // --- mutations the UI invokes ---

    pub fn queue_task(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.run(cx, move |client| client.queue_task(&id));
    }

    pub fn dequeue_task(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.run(cx, move |client| client.dequeue_task(&id));
    }

    pub fn scout_task_now(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.run(cx, move |client| client.scout_task_now(&id));
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.run(cx, move |client| client.set_mode(mode));
    }

    pub fn review_spec(
        &mut self,
        id: SpecId,
        verdict: SpecQueueStatus,
        feedback: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.run(cx, move |client| client.review_spec(&id, verdict, feedback));
    }

    /// 202 from the server; the reply lands via `orchestrator_message` events.
    pub fn send_orchestrator_message(&mut self, content: String, cx: &mut Context<Self>) {
        self.run(cx, move |client| client.send_orchestrator_message(content));
    }

    // --- projections the sections read ---

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| &task.id == id)
    }

    /// The project a task belongs to — for the GitHub link.
    pub fn project(&self, task: &Task) -> Option<&Project> {
        self.projects
            .iter()
            .find(|project| project.id == task.project_id)
    }

    /// Latest spec for a task, if any (specs are append-only per re-scout).
    pub fn latest_spec(&self, id: &TaskId) -> Option<&Spec> {
        self.specs
            .iter()
            .filter(|spec| &spec.task_id == id)
            .max_by_key(|spec| spec.created_at)
    }

    /// The running session for a task, if one is live.
    pub fn running_session(&self, id: &TaskId) -> Option<&Session> {
        self.sessions.iter().find(|session| {
            &session.task_id == id
                && session.status == tasks_client::api::models::SessionStatus::Running
        })
    }
}
