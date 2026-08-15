//! App-wide server state, following the Swift app's `AppModel` and the
//! clients.md contract: one event-stream loop drives everything, events are
//! invalidation signals, and every refresh is a full snapshot of the lists.
//!
//! Threading: `tasks-client` is blocking by design. The SSE iterator lives on
//! its own OS thread and feeds an unbounded channel; a foreground task drains
//! the channel into entity updates. Snapshot fetches and mutations run on
//! gpui's background executor (loopback calls are sub-millisecond).

use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::Context;
use tasks_client::api::events::Event;
use tasks_client::api::http::BriefingStatus;
use tasks_client::api::models::{
    Build, ChatRole, Mode, OrchestratorFeedEvent, OrchestratorMessage, Project, Session, Spec,
    SpecId, SpecQueueItem, SpecQueueStatus, Task, TaskId,
};
use tasks_client::{Client, ClientError, EventStreamItem};

use crate::about;

/// Activity keeps the newest slice, refetched per event — the client never
/// holds the whole log (the Swift app did, to reconstruct joins the API now
/// serves directly).
const ACTIVITY_LIMIT: i64 = 200;
/// Turns loaded when the conversation is first opened. Everything after that
/// arrives incrementally, so the pane keeps growing with the conversation —
/// this bounds the cold start, not the history.
const CHAT_WINDOW: i64 = 200;

/// The provisional view of the orchestrator tick in flight — what the live
/// feed has shown so far, before the durable `orchestrator_messages` row that
/// replaces it exists. Nothing here is persisted or authoritative; it is
/// dropped whole the moment the real reply lands.
pub struct OrchestratorTick {
    /// When *this client* noticed the tick — a client that joins mid-tick
    /// shows the age of what it has seen rather than inventing history it
    /// missed.
    pub started_at: DateTime<Utc>,
    /// Assistant text accumulated from `Delta`, in generation order.
    pub text: String,
    /// The latest tool the agent invoked, if any.
    pub tool: Option<String>,
    /// Newest durable turn at the moment the view opened. An assistant turn
    /// above this watermark is this tick's answer — anything at or below it
    /// is history that was already there.
    since_seq: i64,
}

impl OrchestratorTick {
    fn new(since_seq: i64) -> Self {
        Self {
            started_at: Utc::now(),
            text: String::new(),
            tool: None,
            since_seq,
        }
    }

    /// Nothing has come through yet — the view is open on a wait, not on an
    /// answer in progress.
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.tool.is_none()
    }
}

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
    /// The tick in flight, if one is showing.
    pub orchestrator_tick: Option<OrchestratorTick>,
    /// Bumped on every change to [`Self::orchestrator_tick`]. The chat list
    /// caches item *heights*, so a bubble growing a token at a time has to be
    /// re-spliced to be re-measured — this is what the view compares against.
    pub tick_revision: u64,
    pub mode: Option<Mode>,

    /// The event stream is up. `false` after a drop, until reconnect.
    pub connected: bool,
    /// First snapshot applied — before this the UI shows "connecting".
    pub loaded: bool,
    /// Last transport/API failure worth a banner. Cleared on reconnect and
    /// on any fully-successful refresh.
    pub error: Option<String>,
    /// "This app is older than the server supports" — the answer to a whole
    /// class of failures that otherwise arrive as unrelated decode errors.
    /// Set by the connect-time preflight; outranks [`AppState::error`] in the
    /// banner, because when this app is under the floor, whatever failed
    /// underneath is the symptom.
    pub build_warning: Option<String>,

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
    /// `chat_since` is the newest turn already held: the conversation is
    /// fetched incrementally from there, because a refresh runs on every
    /// SSE event and refetching the whole history each time made transfer
    /// grow as messages x events. `None` opens on the newest window.
    fn fetch(client: &Client, chat_since: Option<i64>) -> Self {
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
            match chat_since {
                Some(since) => client.orchestrator_messages(since),
                None => client.orchestrator_messages_latest(CHAT_WINDOW),
            },
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
        // The About version, not the client crate's own stamp: a warning that
        // names a number the user can't find on screen is most of the way to
        // no warning at all.
        let client = Client::from_env().with_client_version(about::VERSION);

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

        // The orchestrator's live feed, on its own thread and channel rather
        // than merged into the event stream: the two have independent
        // connection lifetimes, and one reconnecting must not disturb the
        // other.
        let (feed_tx, mut feed_rx) = mpsc::unbounded();
        {
            let client = client.clone();
            std::thread::Builder::new()
                .name("tasks-orchestrator-feed".into())
                .spawn(move || {
                    for item in client.stream_orchestrator() {
                        if feed_tx.unbounded_send(item).is_err() {
                            return; // app side gone
                        }
                    }
                })
                .expect("spawn orchestrator-feed thread");
        }
        cx.spawn(async move |this, cx| {
            while let Some(item) = feed_rx.next().await {
                let alive = this
                    .update(cx, |state: &mut AppState, cx| state.on_feed_item(item, cx))
                    .is_ok();
                if !alive {
                    return;
                }
            }
            // The feed ended (a terminal error). Nothing more will arrive, so
            // a view left open here would keep a clock running forever.
            this.update(cx, |state: &mut AppState, cx| {
                state.end_tick(cx);
            })
            .ok();
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
            orchestrator_tick: None,
            tick_revision: 0,
            mode: None,
            connected: false,
            loaded: false,
            error: None,
            build_warning: None,
            refreshing: false,
            dirty: false,
        }
    }

    fn on_stream_item(&mut self, item: EventStreamItem, cx: &mut Context<Self>) {
        match item {
            EventStreamItem::Connected => {
                self.connected = true;
                self.error = None;
                // Every connect, not just the first: a reconnect is usually a
                // server that restarted into a new build, which is exactly
                // when this app can become the stale one.
                self.check_build(cx);
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

    /// One moment of the tick in flight. The feed is ephemeral and lossy by
    /// design — nothing here is state, it is a view of a wait.
    fn on_feed_item(
        &mut self,
        item: Result<OrchestratorFeedEvent, ClientError>,
        cx: &mut Context<Self>,
    ) {
        match item {
            Ok(OrchestratorFeedEvent::Started) => self.on_tick_started(cx),
            // `get_or_insert_with`: an app that connected mid-tick never saw
            // `Started`, and must still show what it can see.
            Ok(OrchestratorFeedEvent::Delta { text }) => {
                let since_seq = self.newest_turn();
                let tick = self
                    .orchestrator_tick
                    .get_or_insert_with(|| OrchestratorTick::new(since_seq));
                tick.text.push_str(&text);
                self.tick_revision += 1;
                cx.notify();
            }
            Ok(OrchestratorFeedEvent::Tool { label }) => {
                let since_seq = self.newest_turn();
                let tick = self
                    .orchestrator_tick
                    .get_or_insert_with(|| OrchestratorTick::new(since_seq));
                // Per the feed's contract in docs/clients.md: text before a
                // tool call is working narration, and the reply is the
                // segment after the last one. Resetting here is what makes
                // the bubble converge on the durable message that replaces
                // it, instead of shrinking to it.
                tick.text.clear();
                tick.tool = Some(label);
                self.tick_revision += 1;
                cx.notify();
            }
            // The durable reply exists now — fetch it. The view is retired by
            // that reply landing, not here: clearing on `Done` would leave a
            // frame where the conversation looks unanswered, and it breaks
            // whenever `Done` is lost.
            Ok(OrchestratorFeedEvent::Done) => self.refresh(cx),
            // Also the server-is-down arm: this fires once per reconnect
            // delay for as long as the server is unreachable, so the refresh
            // is guarded on there having been a live view to resync.
            Err(_) => {
                if self.end_tick(cx) {
                    self.refresh(cx);
                }
            }
        }
    }

    /// The server says a tick began.
    fn on_tick_started(&mut self, cx: &mut Context<Self>) {
        let since_seq = self.newest_turn();
        match &self.orchestrator_tick {
            // The view we opened at send time, still waiting: this is that
            // tick starting. Keep `started_at` — the human's wait began when
            // they hit send, not when the loop got round to them.
            Some(tick) if tick.is_empty() => return,
            // A view a previous tick left behind. A new tick is a new answer,
            // so it starts over rather than appending to the old one.
            _ => self.orchestrator_tick = Some(OrchestratorTick::new(since_seq)),
        }
        self.tick_revision += 1;
        cx.notify();
    }

    /// Open the provisional view unless one is already showing, and report
    /// whether it opened. A second message sent mid-tick must not reset the
    /// clock — the running tick answers both turns.
    fn begin_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.orchestrator_tick.is_some() {
            return false;
        }
        let since_seq = self.newest_turn();
        self.orchestrator_tick = Some(OrchestratorTick::new(since_seq));
        self.tick_revision += 1;
        cx.notify();
        true
    }

    /// Drop the provisional view, reporting whether there was one.
    fn end_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.orchestrator_tick.take().is_none() {
            return false;
        }
        self.tick_revision += 1;
        cx.notify();
        true
    }

    fn newest_turn(&self) -> i64 {
        self.orchestrator_messages
            .last()
            .map(|m| m.seq)
            .unwrap_or(0)
    }

    /// Ask the server whether this build is one it still expects to talk to,
    /// on the background executor. A transport failure is not reported here —
    /// "can't reach the server" is already the disconnect banner's job, and
    /// the verdict is only interesting when the server answered.
    fn check_build(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let check = cx
            .background_executor()
            .spawn(async move { client.preflight() });
        cx.spawn(async move |this, cx| {
            let warning = check.await.ok().and_then(|verdict| verdict.warning());
            this.update(cx, |state: &mut AppState, cx| {
                state.build_warning = warning;
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        // Only what we do not already have. The first pass opens on a
        // window; every later one asks for turns after the newest held.
        let chat_since = self.orchestrator_messages.last().map(|m| m.seq);
        let fetch = cx
            .background_executor()
            .spawn(async move { Snapshot::fetch(&client, chat_since) });
        cx.spawn(async move |this, cx| {
            let snapshot = fetch.await;
            this.update(cx, |state, cx| state.apply(snapshot, cx)).ok();
        })
        .detach();
    }

    fn apply(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        // The first snapshot is history, not news: it backfills old assistant
        // turns, none of which answer a tick that is still running.
        let was_loaded = self.loaded;
        macro_rules! merge {
            ($($field:ident),+) => {
                $(if let Some(value) = snapshot.$field { self.$field = value; })+
            };
        }
        merge!(projects, tasks, sessions, specs, spec_queue, builds, activity, briefings);
        // Appended, not replaced: a refresh carries only the new turns, and
        // history stays in the pane rather than being refetched to sit there.
        if let Some(messages) = snapshot.orchestrator_messages {
            match self.orchestrator_messages.last().map(|m| m.seq) {
                None => self.orchestrator_messages = messages,
                Some(newest) => self
                    .orchestrator_messages
                    .extend(messages.into_iter().filter(|m| m.seq > newest)),
            }
        }
        // Retire the provisional view in the *same* frame that adds its
        // replacement — no flicker, and no dependence on a `Done` that may
        // never come.
        let answered = match &self.orchestrator_tick {
            Some(tick) if was_loaded => self
                .orchestrator_messages
                .iter()
                .any(|m| m.seq > tick.since_seq && m.role == ChatRole::Assistant),
            _ => false,
        };
        if answered {
            self.orchestrator_tick = None;
            self.tick_revision += 1;
        } else if !was_loaded {
            // Rebase the watermark past the backfill we just learned about,
            // or every later refresh would read that history as the answer.
            let newest = self.newest_turn();
            if let Some(tick) = &mut self.orchestrator_tick {
                tick.since_seq = tick.since_seq.max(newest);
            }
        }
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
        self.run_rolling_back(cx, op, |_, _| {});
    }

    /// [`Self::run`], plus a `rollback` that undoes optimistic local state
    /// when the mutation fails — a bubble opened at send time must not
    /// outlive a POST the server never accepted.
    fn run_rolling_back<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Client) -> Result<T, ClientError> + Send + 'static,
        rollback: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let client = self.client.clone();
        let work = cx.background_executor().spawn(async move { op(&client) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |state, cx| match result {
                Ok(_) => state.refresh(cx),
                Err(err) => {
                    rollback(state, cx);
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
    ///
    /// The provisional view opens here rather than waiting for the feed's
    /// `Started`: the round trip to the tick loop is part of what the human
    /// is waiting through, so it belongs on the clock.
    pub fn send_orchestrator_message(&mut self, content: String, cx: &mut Context<Self>) {
        let opened = self.begin_tick(cx);
        self.run_rolling_back(
            cx,
            move |client| client.send_orchestrator_message(content),
            move |state, cx| {
                // Only ours to close. A tick that was already showing is
                // answering someone, and this failure isn't its business.
                if opened {
                    state.end_tick(cx);
                }
            },
        );
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
