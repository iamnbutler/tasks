//! The server process, as something the app can act on.
//!
//! Everything else in this app talks to the server over HTTP. This module is
//! the exception, and the reason for it is narrow: a server cannot gracefully
//! swap itself out through its own API — the process that answers the request
//! is the process being replaced. So the one operation the app cannot do over
//! the wire is exactly the one that matters most after a rebuild, and it is
//! done the way a terminal would do it, by running `tasks reload`.
//!
//! Three things here are more subtle than they look:
//!
//! - **Finding the binary.** `$PATH` is the obvious first answer and the wrong
//!   one: an app launched from the Dock inherits launchd's minimal `PATH`
//!   (`/usr/bin:/bin:/usr/sbin:/sbin`), never a shell's, so `$PATH` finds
//!   `tasks` for exactly the people who would have used a terminal anyway. The
//!   pidfile's `exe` is both the most likely to exist and the most obviously
//!   correct thing to restart — it is *the binary that is serving*. A bundle
//!   installed by `make dist` carries a `tasks` at `Contents/Helpers/tasks`,
//!   and that is the answer when nothing is serving yet — which on an end
//!   user's machine is the first launch. See [`resolve_binary`].
//! - **What the ops mean depends on what was found.** A seed from the bundle
//!   turns the building ops into `tasks service install` — the one-button
//!   install: copy to `~/.tasks/bin`, register the LaunchAgent, start. For
//!   everything else the ops stay `reload`/`stop`, with `--no-build` derived
//!   from the binary itself, never a setting: a `tasks` with no workspace
//!   above it has nothing to build in, so it swaps itself in — and when it
//!   is the service's own binary, `reload` and `stop` delegate to launchctl
//!   on their own. See [`args_for`] and [`build_in_place`].
//! - **The child's `PATH`.** The same trap bites twice: `tasks reload` runs
//!   `cargo build` as its first step, and `cargo` is not on that `PATH`
//!   either. Hence [`child_path`], and hence [`which`] searching the *child's*
//!   `PATH` rather than this process's — resolving a binary somewhere the
//!   child would not look is how you resolve one and then fail to run it.
//! - **Draining both pipes.** A `cargo build` prints far more than a pipe
//!   buffer holds, so [`pump`] reads stdout and stderr on separate threads
//!   *before* `wait()`. Reading one, or waiting first, deadlocks.
//!
//! The model holds no rendering. `server_window` shows it.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, Global};
use tasks_client::api::http::{DeviceFlowStatus, InFlight, ServerStatus};
use tasks_client::api::models::Mode;
use tasks_client::api::version::VersionInfo;
use tasks_client::Client;

/// How much of a run's output is kept. A `cargo build` of the whole workspace
/// is thousands of lines and only the tail explains anything; the head is
/// dropped so a long build cannot grow the app's memory without bound.
const MAX_LINES: usize = 500;

/// Overrides which `tasks` binary the menu drives.
const BIN_ENV: &str = "TASKS_BIN";

/// The workspace `tasks reload` builds in, when it cannot find one itself.
const REPO_ENV: &str = "TASKS_REPO";

/// Directories prepended to the child's `PATH`, so `cargo` is findable from an
/// app launched by launchd. Homebrew (arm and intel) and rustup's default.
const PATH_PREFIXES: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// What the menu can ask of the server process.
///
/// There is deliberately no `Start`: `tasks reload` with no live pid already
/// *is* a start, so a second op would run an identical command and differ only
/// in its label. The label is the part worth having, so the menu *item*
/// changes its name (see [`Op::label`]) and the op does not multiply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `tasks reload` — refuses if a scout or a build is in flight.
    Restart,
    /// `tasks reload --when-idle` — waits for a drain point instead.
    RestartWhenIdle,
    /// `tasks reload --force` — swaps regardless of what is running.
    RestartAnyway,
    /// `tasks stop` — immediate, and the one op this window asks about first.
    Stop,
    /// `tasks stop --when-idle` — waits for a drain point, then stops, and
    /// leaves dispatch paused.
    StopWhenIdle,
}

impl Op {
    /// The arguments to `tasks`, `--repo` aside.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Op::Restart => &["reload"],
            Op::RestartWhenIdle => &["reload", "--when-idle"],
            Op::RestartAnyway => &["reload", "--force"],
            Op::Stop => &["stop"],
            Op::StopWhenIdle => &["stop", "--when-idle"],
        }
    }

    /// Whether this op ends the server rather than replacing it. Drives the
    /// wording of every verdict a stop and a restart do not share.
    pub fn stops(self) -> bool {
        matches!(self, Op::Stop | Op::StopWhenIdle)
    }

    /// Whether this op builds first — and so needs a workspace to build in.
    /// `stop` rejects unknown flags, so `--repo` is passed only to the ops
    /// that would use it.
    pub fn builds(self) -> bool {
        !self.stops()
    }

    /// Whether to ask before running it with work in flight.
    ///
    /// Only the immediate `Stop`: it is the one op that ends the process under
    /// running work with nothing to hand it to. Every other op either refuses
    /// on its own (`Restart`, exit 3), waits (`--when-idle`), or has already
    /// been told twice (`--force`).
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Op::Stop)
    }

    /// How the run names itself while it is running.
    pub fn label(self) -> &'static str {
        match self {
            Op::Restart => "Restarting the server",
            Op::RestartWhenIdle => "Restarting when idle",
            Op::RestartAnyway => "Restarting anyway",
            Op::Stop => "Stopping the server",
            Op::StopWhenIdle => "Stopping when idle",
        }
    }
}

/// What a `Command` would run, as one line — the log's first entry, so a run
/// that fails says what it ran and can be re-run in a terminal. Rendered from
/// the command itself rather than rebuilt from the op, because a line that
/// omits a flag the child received is worse than no line at all.
fn command_line(command: &Command) -> String {
    let mut line = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

/// What a finished run earned. The codes are `ReloadError::exit_code`'s, and
/// they are the whole reason a GUI can say something useful about a failure
/// instead of showing a wall of stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Exit 1. Overwhelmingly a failed build — which is the one failure that
    /// costs nothing, because `reload` builds before it signals anything.
    BuildFailed,
    /// Exit 3: work in flight, and a restart would destroy it.
    Busy,
    /// Exit 4: `--when-idle` gave up waiting.
    DrainTimeout,
    /// Exit 5: the old server stopped and the new one did not come up.
    SwapFailed,
    /// Some other exit code, reported as itself rather than guessed at.
    Failed(i32),
    /// Killed by a signal — no exit code at all.
    Killed,
    /// The binary would not even spawn.
    CouldNotRun,
}

/// Exit code → verdict. Pure, and the only place these numbers are read.
pub fn verdict(code: Option<i32>) -> Outcome {
    match code {
        Some(0) => Outcome::Done,
        Some(1) => Outcome::BuildFailed,
        Some(3) => Outcome::Busy,
        Some(4) => Outcome::DrainTimeout,
        Some(5) => Outcome::SwapFailed,
        Some(other) => Outcome::Failed(other),
        None => Outcome::Killed,
    }
}

impl Outcome {
    /// One sentence naming the stage that decided this, in the terms the
    /// operator cares about: what happened to the *running* server.
    pub fn headline(self, op: Op) -> String {
        match (self, op) {
            (Outcome::Done, Op::Stop) => "Stopped. Nothing is serving.".to_string(),
            // The one lasting consequence of waiting: nothing follows a stop
            // that could carry the mode, and no boot resumes the stored one.
            (Outcome::Done, Op::StopWhenIdle) => {
                "Stopped once the work had landed. Nothing is serving, and dispatch \
                 is left paused."
                    .to_string()
            }
            (Outcome::Done, _) => "Up. The new build is serving.".to_string(),
            (Outcome::BuildFailed, _) => {
                "The build failed; the running server was not touched.".to_string()
            }
            // A stop cannot be refused for having work in flight — it is the
            // way *through* that. Exit 3 for a stop means something else: the
            // server would not say what is in flight, so there is no way to
            // tell when it is idle.
            (Outcome::Busy, op) if op.stops() => {
                "Refused: the server would not say what is in flight, so there is no \
                 way to tell when it is idle. Stop it now instead."
                    .to_string()
            }
            (Outcome::Busy, _) => {
                "Refused: work is in flight that a restart would destroy.".to_string()
            }
            (Outcome::DrainTimeout, op) if op.stops() => {
                "Gave up waiting for a drain point; nothing was stopped.".to_string()
            }
            (Outcome::DrainTimeout, _) => {
                "Gave up waiting for a drain point; nothing was restarted.".to_string()
            }
            (Outcome::SwapFailed, _) => {
                "The swap did not land — the old server is gone and the new one \
                 is not answering."
                    .to_string()
            }
            (Outcome::Failed(code), _) => format!("Exited {code}."),
            (Outcome::Killed, _) => "Killed before it finished.".to_string(),
            (Outcome::CouldNotRun, _) => {
                "Could not run the tasks binary — set TASKS_BIN to its path.".to_string()
            }
        }
    }

    /// Whether the server was left running and untouched. Only these two
    /// verdicts can promise that, and only for them is "restart anyway" the
    /// obvious next thing to offer.
    pub fn is_refusal(self) -> bool {
        matches!(self, Outcome::Busy)
    }

    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Done)
    }
}

/// `$TASKS_BIN`, else the binary the running server published, else the
/// `tasks` riding in this app's own bundle, else `tasks` on the child's
/// `PATH`. See the module docs for why that order.
///
/// The bundle sits *below* the pidfile deliberately: the binary that is
/// serving is the most obviously correct thing to restart, and after a bundle
/// update the two agree anyway — the install path is stable, so the pidfile's
/// `exe` names the new binary at the old path.
pub fn resolve_binary(data_dir: Option<&Path>) -> ResolvedBinary {
    resolve_binary_with(
        std::env::var_os(BIN_ENV).map(PathBuf::from),
        data_dir,
        bundled_binary(),
        || which("tasks"),
    )
}

/// What resolution found, plus the one fact about *how* that changes what
/// the ops mean: a binary that came from this app's own bundle is a **seed**
/// — nothing is serving and nothing is installed — so the building ops run
/// `tasks service install` (copy it to `~/.tasks/bin`, register the
/// LaunchAgent, start) rather than `reload`, which would leave the bundle as
/// the serving binary's home and hand its lifetime to the next app update.
pub struct ResolvedBinary {
    pub path: PathBuf,
    pub from_bundle: bool,
}

/// [`resolve_binary`]'s decision, with its environmental inputs handed in so
/// every branch is testable without touching the environment.
fn resolve_binary_with(
    explicit: Option<PathBuf>,
    data_dir: Option<&Path>,
    bundled: Option<PathBuf>,
    on_path: impl FnOnce() -> Option<PathBuf>,
) -> ResolvedBinary {
    let found = |path| ResolvedBinary {
        path,
        from_bundle: false,
    };
    if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return found(path);
    }
    if let Some(dir) = data_dir {
        // `is_file` matters: a pidfile outlives the binary it names (a
        // `target/debug/tasks` that was `cargo clean`ed), and a stale record
        // must fall through rather than resolve to nothing runnable.
        if let Some(file) = tasks_api::paths::read_pid_file(dir) {
            if file.exe.is_file() {
                return found(file.exe);
            }
        }
    }
    if let Some(path) = bundled {
        return ResolvedBinary {
            path,
            from_bundle: true,
        };
    }
    // Last resort, and a bare name on purpose: unresolved, the child looks it
    // up in the `PATH` it will actually run with.
    found(on_path().unwrap_or_else(|| PathBuf::from("tasks")))
}

/// The `tasks` binary a `make dist` bundle carries, at
/// `Tasks.app/Contents/Helpers/tasks`. `None` in a dev bundle, which carries
/// only the app — that absence is what keeps dev behaviour identical.
fn bundled_binary() -> Option<PathBuf> {
    bundled_binary_from(&std::env::current_exe().ok()?)
}

/// [`bundled_binary`] with the executable handed in, so the layout rule is
/// testable.
///
/// `Helpers`, and never a sibling in `Contents/MacOS`: the app binary there
/// is `Tasks`, and the default macOS filesystem is case-insensitive, so a
/// sibling probe for `tasks` finds *this app itself* and the menu would spawn
/// the GUI with `reload` for arguments. The same collision is why `make
/// dist` installs the server binary into its own directory.
fn bundled_binary_from(exe: &Path) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    let candidate = contents.join("Helpers/tasks");
    candidate.is_file().then_some(candidate)
}

/// Whether `binary` has a workspace to build in — the fact that decides
/// between `tasks reload` (dev: build, then swap in what was built) and
/// `tasks reload --no-build` (installed: swap the binary in as it is).
///
/// Derived, never configured: the probe is the one `reload` itself uses to
/// find a workspace (`crates/tasks/Cargo.toml` above the binary), so the app
/// never asks for a build the child would fail to locate. A `$TASKS_REPO`
/// override means the operator has named a workspace explicitly, and that is
/// a request to build in it. The bare-name fallback (nothing resolved,
/// relative path) keeps building, as it always did — `reload` falls back to
/// its own cwd detection there.
fn build_in_place(binary: &Path, repo_override: bool) -> bool {
    if repo_override {
        return true;
    }
    if !binary.is_absolute() {
        return true;
    }
    binary
        .ancestors()
        .any(|dir| dir.join("crates/tasks/Cargo.toml").is_file())
}

/// `PATH` for the child: the usual toolchain locations, then whatever this
/// process inherited.
fn child_path() -> OsString {
    let mut dirs: Vec<PathBuf> = PATH_PREFIXES.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    dirs.extend(std::env::split_paths(&inherited));
    std::env::join_paths(dirs).unwrap_or(inherited)
}

/// `name` on the *child's* `PATH`, not this process's.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&child_path())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The command one op runs, fully specified: binary, args, `PATH`, and the
/// data dir the app itself is talking about.
pub fn command_for(op: Op, data_dir: Option<&Path>) -> Command {
    let resolved = resolve_binary(data_dir);
    let repo = std::env::var_os(REPO_ENV).filter(|r| !r.is_empty());
    let build = build_in_place(&resolved.path, repo.is_some());
    let mut command = Command::new(&resolved.path);
    command.args(args_for(op, resolved.from_bundle, repo.as_deref(), build));
    command.env("PATH", child_path());
    if let Some(dir) = data_dir {
        // Be explicit about which server this is: the app resolved the data
        // dir to find the pidfile, and the child must not resolve a different
        // one out of a differently-populated environment.
        command.env(tasks_api::paths::DATA_DIR_ENV, dir);
    }
    command
}

/// The full argument list for one op.
///
/// `from_bundle` first, because it changes the *verb*: the bundle's binary
/// is a seed, so a building op runs `tasks service install` — copy to
/// `~/.tasks/bin`, register the LaunchAgent, start — the one-button install.
/// Anything less (a plain reload from inside the bundle) would make the
/// bundle the serving binary's home, and an app update would then delete the
/// server out from under a live pipeline, which is the cargo-clean failure
/// this design exists to end. The stop ops stay themselves: with the bundle
/// winning resolution nothing is serving, and `tasks stop` says so.
///
/// `--repo` is the escape hatch for the workspace `reload` builds in: it is
/// found from the cwd, else from the `tasks` binary's ancestors, and a
/// bundled app's cwd is `/` which the child inherits. The ancestors path
/// answers for `<repo>/target/debug/tasks` and not for an installed binary
/// outside a checkout. It goes only to the ops that build — `stop` rejects
/// unknown flags.
///
/// `build: false` turns a building op into `--no-build`: the resolved binary
/// swaps *itself* in — and when that binary is the launchd service's,
/// `reload` delegates to launchctl on its own. `--repo` is meaningless
/// without a build and never rides with it — [`build_in_place`] returns
/// `true` whenever a repo was named, so the combination cannot arise from
/// [`command_for`].
fn args_for(op: Op, from_bundle: bool, repo: Option<&OsStr>, build: bool) -> Vec<OsString> {
    if from_bundle && op.builds() {
        return vec![OsString::from("service"), OsString::from("install")];
    }
    let mut args: Vec<OsString> = op.args().iter().map(OsString::from).collect();
    if op.builds() && !build {
        args.push(OsString::from("--no-build"));
        return args;
    }
    if let Some(repo) = repo.filter(|_| op.builds()) {
        args.push(OsString::from("--repo"));
        args.push(repo.to_os_string());
    }
    args
}

/// One line of a run's output, or its verdict. The verdict always arrives
/// last and exactly once.
enum RunItem {
    Line(String),
    Finished(Outcome),
}

/// Run `command` to completion, streaming its output into `tx`.
///
/// Blocking — call it on its own thread. Both pipes are drained on threads of
/// their own *before* `wait()`: a `cargo build` prints far more than a pipe
/// buffer holds, so reading one pipe, or waiting first, deadlocks.
fn pump(mut command: Command, tx: mpsc::UnboundedSender<RunItem>) {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.unbounded_send(RunItem::Line(err.to_string()));
            let _ = tx.unbounded_send(RunItem::Finished(Outcome::CouldNotRun));
            return;
        }
    };

    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(reader(stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(reader(stderr, tx.clone()));
    }

    let status = child.wait();
    // Only after both pipes hit EOF is the output complete; joining after
    // `wait` is what keeps the verdict the last thing sent.
    for reader in readers {
        let _ = reader.join();
    }
    let outcome = match status {
        Ok(status) => verdict(status.code()),
        Err(err) => {
            let _ = tx.unbounded_send(RunItem::Line(err.to_string()));
            Outcome::CouldNotRun
        }
    };
    let _ = tx.unbounded_send(RunItem::Finished(outcome));
}

/// A thread that turns one pipe into lines on the channel.
fn reader(
    pipe: impl std::io::Read + Send + 'static,
    tx: mpsc::UnboundedSender<RunItem>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Lossy, not `Result`: cargo's diagnostics are the only thing likely
        // to carry odd bytes, and dropping the line would lose the error.
        for line in BufReader::new(pipe).split(b'\n') {
            match line {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).trim_end().to_string();
                    if tx.unbounded_send(RunItem::Line(text)).is_err() {
                        return; // app side gone
                    }
                }
                Err(_) => return,
            }
        }
    })
}

/// One invocation of `tasks`: what it is, when it started, what it has said,
/// and how it ended.
pub struct Run {
    pub op: Op,
    pub started_at: DateTime<Utc>,
    /// Output in arrival order, capped at [`MAX_LINES`] from the front.
    pub lines: VecDeque<String>,
    /// `None` while it is still running.
    pub outcome: Option<Outcome>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Run {
    fn new(op: Op, command_line: String) -> Self {
        let mut lines = VecDeque::new();
        lines.push_back(format!("$ {command_line}"));
        Self {
            op,
            started_at: Utc::now(),
            lines,
            outcome: None,
            finished_at: None,
        }
    }

    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// The app's handle on the server *process* — the current or last run, plus
/// the last `/status` and `/version` it read.
///
/// A global rather than workspace state: the Server window has no workspace
/// behind it, and the menu's handlers are global too.
pub struct ServerControl {
    client: Client,
    /// The run in flight, or the last one that finished. Kept after it ends —
    /// the verdict is most of the value, and it must survive the run.
    pub run: Option<Run>,
    /// An op parked on a question — see [`ServerControl::request`]. At most
    /// one, and it never survives a run starting.
    pub pending: Option<Op>,
    pub status: Option<ServerStatus>,
    pub version: Option<VersionInfo>,
    /// Why the last probe failed. Not an error banner: "nothing is serving"
    /// is a state this window exists to produce.
    pub probe_error: Option<String>,
    /// Why the last mode write failed. Kept apart from [`Self::probe_error`]
    /// so the next poll — which is a second away — cannot quietly erase it.
    pub mode_error: Option<String>,
    pub probed_at: Option<DateTime<Utc>>,
    /// Where this app thinks the server lives — shown, because a surprising
    /// answer here explains every other surprising answer.
    pub data_dir: Option<PathBuf>,
    probing: bool,
    /// The GitHub sign-in (#1061): what `GET /auth/github/device` last
    /// answered. `None` while the server is unreachable, predates the route,
    /// or has not been asked yet — the row simply does not render, because
    /// the Server row above already says what is wrong.
    pub sign_in: Option<DeviceFlowStatus>,
    /// Why the last *start* failed — user-initiated, so it is shown beside
    /// the button, unlike the poll's own failures, which just empty the row.
    pub sign_in_error: Option<String>,
    sign_in_probing: bool,
}

/// The global wrapper. gpui globals are values, so the entity lives inside
/// one and the observers hang off the entity.
struct GlobalServerControl(Entity<ServerControl>);

impl Global for GlobalServerControl {}

/// Create the global. Call before `menus::init`, which reads it.
pub fn init(cx: &mut App) {
    let control = cx.new(ServerControl::new);
    cx.set_global(GlobalServerControl(control));
}

impl ServerControl {
    fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            client: Client::from_env(),
            run: None,
            pending: None,
            status: None,
            version: None,
            probe_error: None,
            mode_error: None,
            probed_at: None,
            data_dir: tasks_api::paths::data_dir(),
            probing: false,
            sign_in: None,
            sign_in_error: None,
            sign_in_probing: false,
        }
    }

    /// The global instance. Panics if [`init`] has not run.
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalServerControl>().0.clone()
    }

    /// A run is in flight.
    pub fn busy(&self) -> bool {
        self.run.as_ref().is_some_and(Run::is_running)
    }

    /// Work the last probe saw that ending the process would destroy.
    ///
    /// A poll, up to [`crate::server_window::POLL`] stale, so what it feeds is
    /// a question and never a lock — `--when-idle` remains the only thing that
    /// actually guarantees a drain.
    pub fn destructible(&self) -> Option<&InFlight> {
        self.status
            .as_ref()
            .map(|status| &status.in_flight)
            .filter(|in_flight| in_flight.is_destructible())
    }

    /// Ask for `op`: start it, or park it as a question first.
    ///
    /// The parking is what a GUI can do that `tasks stop` cannot — the CLI's
    /// plain stop is deliberately ungated (scripts and `make stop` depend on
    /// it), so the confirmation lives here, where there is somewhere to ask.
    ///
    /// Returns whether a run started.
    pub fn request(&mut self, op: Op, cx: &mut Context<Self>) -> bool {
        if self.busy() {
            return false;
        }
        if op.needs_confirmation() && self.destructible().is_some() {
            self.pending = Some(op);
            cx.notify();
            return false;
        }
        self.start(op, cx)
    }

    /// Drop the parked question, having answered it or thought better of it.
    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending.take().is_some() {
            cx.notify();
        }
    }

    /// Start `op`, unless a run is already in flight.
    ///
    /// The refusal is the real protection against two concurrent runs; the
    /// menu greying the items out only exists so the menu can say why instead
    /// of swallowing the click.
    pub fn start(&mut self, op: Op, cx: &mut Context<Self>) -> bool {
        if self.busy() {
            return false;
        }
        // Whatever was parked, this answers it: the question is about work in
        // flight, and something is now acting on it.
        self.pending = None;
        let command = command_for(op, self.data_dir.as_deref());
        let line = command_line(&command);
        let (tx, mut rx) = mpsc::unbounded();
        // The channel is the only link to the child: if the app side goes
        // away the pump's sends simply fail and the child runs on. That is
        // the right way round — a swap in flight must not depend on anyone
        // still watching it.
        std::thread::Builder::new()
            .name("tasks-server-op".into())
            .spawn(move || pump(command, tx))
            .expect("spawn server-op thread");
        self.run = Some(Run::new(op, line));
        cx.spawn(async move |this, cx| {
            while let Some(item) = rx.next().await {
                let alive = this
                    .update(cx, |this: &mut ServerControl, cx| this.on_item(item, cx))
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();
        cx.notify();
        true
    }

    fn on_item(&mut self, item: RunItem, cx: &mut Context<Self>) {
        let Some(run) = self.run.as_mut() else { return };
        match item {
            RunItem::Line(line) => run.push(line),
            RunItem::Finished(outcome) => {
                run.outcome = Some(outcome);
                run.finished_at = Some(Utc::now());
                // The world moved: whatever is serving now (including
                // nothing) is what the window should be showing.
                self.refresh(cx);
            }
        }
        cx.notify();
    }

    /// Re-read `/status` and `/version` on the background executor.
    ///
    /// Coalesced, because the window polls on a timer and a wedged server can
    /// hold a probe for the client's whole call timeout.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // The sign-in rides the same cadence: one more loopback GET per poll,
        // coalesced on its own flag so a wedged server cannot stack probes.
        self.refresh_sign_in(cx);
        if self.probing {
            return;
        }
        self.probing = true;
        let client = self.client.clone();
        let probe = cx
            .background_executor()
            .spawn(async move { (client.status(), client.server_version()) });
        cx.spawn(async move |this, cx| {
            let (status, version) = probe.await;
            this.update(cx, |this: &mut ServerControl, cx| {
                this.probing = false;
                match status {
                    Ok(status) => {
                        this.status = Some(status);
                        this.probe_error = None;
                    }
                    Err(err) => {
                        this.status = None;
                        this.probe_error = Some(err.to_string());
                    }
                }
                // The question was about work in flight; with the work landed
                // it has no subject left, so it collapses and the click that
                // was about to answer it is dropped. Stopping later on a
                // question nobody is being asked any more would be worse.
                if this.destructible().is_none() {
                    this.pending = None;
                }
                // A server that answers `/status` but not `/version` predates
                // the route; keeping the last known one would be a lie about
                // the build that is serving now.
                this.version = version.ok();
                this.probed_at = Some(Utc::now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the sign-in status (#1061). Quiet on failure: a poll error
    /// empties the row rather than raising anything — an unreachable server
    /// and one predating the route both read as "nothing to say", and the
    /// Server row is where unreachability is already reported.
    pub fn refresh_sign_in(&mut self, cx: &mut Context<Self>) {
        if self.sign_in_probing {
            return;
        }
        self.sign_in_probing = true;
        let client = self.client.clone();
        let probe = cx
            .background_executor()
            .spawn(async move { client.github_device_flow() });
        cx.spawn(async move |this, cx| {
            let result = probe.await;
            this.update(cx, |this: &mut ServerControl, cx| {
                this.sign_in_probing = false;
                this.sign_in = result.ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Start (or supersede) the GitHub sign-in. The server answers
    /// already-`Pending` with the code, so one round trip puts the code on
    /// screen; the server polls GitHub and seals the token itself, and this
    /// app only ever sees status.
    pub fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let work = cx
            .background_executor()
            .spawn(async move { client.start_github_device_flow() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this: &mut ServerControl, cx| {
                match result {
                    Ok(status) => {
                        this.sign_in = Some(status);
                        this.sign_in_error = None;
                    }
                    Err(err) => this.sign_in_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Set the pipeline mode. The Server window is a separate window with no
    /// workspace behind it, so it cannot route this through `AppState` —
    /// the `mode_changed` event it produces is what puts the two back in
    /// step.
    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let work = cx
            .background_executor()
            .spawn(async move { client.set_mode(mode) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this: &mut ServerControl, cx| {
                this.mode_error = result.err().map(|err| err.to_string());
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The mode the last probe saw, if any.
    pub fn mode(&self) -> Option<Mode> {
        self.status.as_ref().map(|status| status.mode)
    }

    /// Something is serving right now, as far as the last probe knows.
    pub fn serving(&self) -> bool {
        self.status.is_some()
    }
}

/// `open -R` on macOS, `xdg-open` elsewhere: reveal `path` in the file
/// manager. Best-effort — a menu item that reveals nothing is not worth an
/// error dialog.
pub fn reveal(path: &Path) {
    let (program, args): (&str, Vec<&OsStr>) = if cfg!(target_os = "macos") {
        ("open", vec![OsStr::new("-R"), path.as_os_str()])
    } else {
        ("xdg-open", vec![path.as_os_str()])
    };
    let _ = Command::new(program)
        .args(args)
        .env("PATH", child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Open `path` itself (a directory, in this app's use).
pub fn open_path(path: &Path) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(program)
        .arg(path)
        .env("PATH", child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ops_map_to_the_cli_they_name() {
        assert_eq!(Op::Restart.args(), ["reload"]);
        assert_eq!(Op::RestartWhenIdle.args(), ["reload", "--when-idle"]);
        assert_eq!(Op::RestartAnyway.args(), ["reload", "--force"]);
        assert_eq!(Op::Stop.args(), ["stop"]);
        assert_eq!(Op::StopWhenIdle.args(), ["stop", "--when-idle"]);
    }

    /// Only the immediate stop asks first: every other op either refuses on
    /// its own, waits, or has already been told twice.
    #[test]
    fn only_an_immediate_stop_asks_first() {
        assert!(Op::Stop.needs_confirmation());
        assert!(!Op::StopWhenIdle.needs_confirmation());
        assert!(!Op::Restart.needs_confirmation());
        assert!(!Op::RestartWhenIdle.needs_confirmation());
        assert!(!Op::RestartAnyway.needs_confirmation());
    }

    #[test]
    fn the_ops_that_end_the_server_know_they_do() {
        assert!(Op::Stop.stops());
        assert!(Op::StopWhenIdle.stops());
        assert!(!Op::Restart.stops());
        assert!(!Op::RestartWhenIdle.stops());
        assert!(!Op::RestartAnyway.stops());
    }

    /// `stop` rejects unknown flags, so `--repo` must never reach it.
    #[test]
    fn only_the_ops_that_build_take_a_repo() {
        assert!(Op::Restart.builds());
        assert!(Op::RestartWhenIdle.builds());
        assert!(Op::RestartAnyway.builds());
        assert!(!Op::Stop.builds());
        assert!(!Op::StopWhenIdle.builds());

        let repo = OsString::from("/w/tasks");
        assert_eq!(
            args_for(Op::Restart, false, Some(&repo), true),
            ["reload", "--repo", "/w/tasks"]
        );
        assert_eq!(args_for(Op::Stop, false, Some(&repo), true), ["stop"]);
        assert_eq!(
            args_for(Op::StopWhenIdle, false, Some(&repo), true),
            ["stop", "--when-idle"]
        );
        assert_eq!(args_for(Op::Restart, false, None, true), ["reload"]);
    }

    /// An installed binary has no workspace, so every building op turns into
    /// `--no-build` — the resolved binary swaps itself in — and the ops that
    /// never build are untouched.
    #[test]
    fn without_a_workspace_building_ops_go_no_build() {
        assert_eq!(
            args_for(Op::Restart, false, None, false),
            ["reload", "--no-build"]
        );
        assert_eq!(
            args_for(Op::RestartWhenIdle, false, None, false),
            ["reload", "--when-idle", "--no-build"]
        );
        assert_eq!(
            args_for(Op::RestartAnyway, false, None, false),
            ["reload", "--force", "--no-build"]
        );
        assert_eq!(args_for(Op::Stop, false, None, false), ["stop"]);
        assert_eq!(
            args_for(Op::StopWhenIdle, false, None, false),
            ["stop", "--when-idle"]
        );
    }

    /// The log's first line is the command as the child received it, so a
    /// failed run can be re-run in a terminal verbatim.
    #[test]
    fn the_log_opens_with_what_was_actually_run() {
        let mut command = Command::new("/usr/local/bin/tasks");
        command.args(args_for(
            Op::RestartWhenIdle,
            false,
            Some(OsStr::new("/w/tasks")),
            true,
        ));
        assert_eq!(
            command_line(&command),
            "/usr/local/bin/tasks reload --when-idle --repo /w/tasks"
        );
    }

    #[test]
    fn exit_codes_become_verdicts() {
        assert_eq!(verdict(Some(0)), Outcome::Done);
        assert_eq!(verdict(Some(1)), Outcome::BuildFailed);
        assert_eq!(verdict(Some(3)), Outcome::Busy);
        assert_eq!(verdict(Some(4)), Outcome::DrainTimeout);
        assert_eq!(verdict(Some(5)), Outcome::SwapFailed);
        assert_eq!(verdict(Some(9)), Outcome::Failed(9));
        assert_eq!(verdict(None), Outcome::Killed);
    }

    /// The one verdict that promises the running server is untouched is the
    /// one the window grows buttons for.
    #[test]
    fn only_a_refusal_offers_a_way_forward() {
        assert!(Outcome::Busy.is_refusal());
        assert!(!Outcome::DrainTimeout.is_refusal());
        assert!(!Outcome::SwapFailed.is_refusal());
        assert!(!Outcome::Done.is_refusal());
    }

    #[test]
    fn a_failed_build_says_the_server_was_not_touched() {
        let text = Outcome::BuildFailed.headline(Op::Restart);
        assert!(text.contains("was not touched"), "{text}");
    }

    #[test]
    fn a_stop_and_a_restart_do_not_share_a_verdict_sentence() {
        assert_ne!(
            Outcome::Done.headline(Op::Stop),
            Outcome::Done.headline(Op::Restart)
        );
    }

    /// The three verdicts a stop and a restart cannot phrase the same way: a
    /// waited-out stop leaves the pipeline paused, a stop that gave up stopped
    /// nothing, and exit 3 means something else entirely for a stop.
    #[test]
    fn a_stop_says_what_a_restart_cannot() {
        let done = Outcome::Done.headline(Op::StopWhenIdle);
        assert!(done.contains("paused"), "{done}");

        let timeout = Outcome::DrainTimeout.headline(Op::StopWhenIdle);
        assert!(timeout.contains("nothing was stopped"), "{timeout}");
        assert!(Outcome::DrainTimeout
            .headline(Op::RestartWhenIdle)
            .contains("nothing was restarted"));

        let busy = Outcome::Busy.headline(Op::StopWhenIdle);
        assert!(busy.contains("what is in flight"), "{busy}");
        assert_ne!(busy, Outcome::Busy.headline(Op::Restart));
    }

    #[test]
    fn an_explicit_binary_wins() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = PathBuf::from("/somewhere/tasks");
        let resolved = resolve_binary_with(Some(explicit.clone()), Some(dir.path()), None, || {
            panic!("must not fall through")
        });
        assert_eq!(resolved.path, explicit);
        assert!(!resolved.from_bundle);
    }

    #[test]
    fn the_serving_binary_beats_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tasks-binary");
        std::fs::File::create(&exe).unwrap();
        write_pidfile(dir.path(), &exe);
        assert_eq!(
            resolve_binary_with(None, Some(dir.path()), None, || panic!(
                "must not fall through"
            ))
            .path,
            exe
        );
    }

    /// The binary that is serving is the most obviously correct thing to
    /// restart, even from an app that carries its own — and after a bundle
    /// update the two are the same path anyway.
    #[test]
    fn the_serving_binary_beats_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tasks-binary");
        std::fs::File::create(&exe).unwrap();
        write_pidfile(dir.path(), &exe);
        let resolved = resolve_binary_with(
            None,
            Some(dir.path()),
            Some(PathBuf::from(
                "/Applications/Tasks.app/Contents/Helpers/tasks",
            )),
            || panic!("must not fall through"),
        );
        assert_eq!(resolved.path, exe);
        assert!(
            !resolved.from_bundle,
            "the serving binary is a server to reload, not a seed to install"
        );
    }

    /// With nothing serving — the end user's first launch — the bundle's own
    /// binary is the answer, never a `PATH` that launchd populated.
    #[test]
    fn the_bundle_beats_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = PathBuf::from("/Applications/Tasks.app/Contents/Helpers/tasks");
        let resolved = resolve_binary_with(None, Some(dir.path()), Some(bundled.clone()), || {
            panic!("must not fall through")
        });
        assert_eq!(resolved.path, bundled);
        assert!(
            resolved.from_bundle,
            "a seed win is what turns the ops into `service install`"
        );
    }

    /// A pidfile outlives the binary it names; a record naming a path that is
    /// gone must fall through rather than resolve to something unrunnable.
    #[test]
    fn a_stale_record_falls_through_to_the_path() {
        let dir = tempfile::tempdir().unwrap();
        write_pidfile(dir.path(), Path::new("/nonexistent/tasks"));
        assert_eq!(
            resolve_binary_with(None, Some(dir.path()), None, || Some(PathBuf::from(
                "/usr/bin/tasks"
            )))
            .path,
            PathBuf::from("/usr/bin/tasks")
        );
    }

    #[test]
    fn with_nothing_to_go_on_it_is_a_bare_name_for_the_child_to_resolve() {
        let resolved = resolve_binary_with(None, None, None, || None);
        assert_eq!(resolved.path, PathBuf::from("tasks"));
        assert!(!resolved.from_bundle);
    }

    /// The one-button install: a seed win rewrites the building ops into
    /// `service install`, and only those — a stop against a seed has nothing
    /// to stop and stays `tasks stop`, which says "not serving".
    #[test]
    fn seed_restarts_become_a_service_install() {
        for op in [Op::Restart, Op::RestartWhenIdle, Op::RestartAnyway] {
            assert_eq!(
                args_for(op, true, None, false),
                ["service", "install"],
                "{op:?}"
            );
        }
        assert_eq!(args_for(Op::Stop, true, None, false), ["stop"]);
        assert_eq!(
            args_for(Op::StopWhenIdle, true, None, false),
            ["stop", "--when-idle"]
        );
    }

    /// The layout rule: the server binary lives in `Contents/Helpers`, and a
    /// bundle without one — every dev bundle — yields nothing. The probe must
    /// never look for a `tasks` sibling in `Contents/MacOS`: on the default
    /// (case-insensitive) macOS filesystem that path *is* the `Tasks` app
    /// binary, and resolving it would have the menu spawn the GUI with
    /// `reload` for arguments.
    #[test]
    fn the_bundled_server_is_found_in_helpers_and_only_there() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("Tasks.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        let app_exe = contents.join("MacOS/Tasks");
        std::fs::File::create(&app_exe).unwrap();

        // A dev bundle: only the app binary. Nothing to find — and on a
        // case-insensitive filesystem this is exactly the layout where a
        // sibling probe would find the app itself.
        assert_eq!(bundled_binary_from(&app_exe), None);

        std::fs::create_dir_all(contents.join("Helpers")).unwrap();
        let server = contents.join("Helpers/tasks");
        std::fs::File::create(&server).unwrap();
        assert_eq!(bundled_binary_from(&app_exe), Some(server));
    }

    /// The build decision is derived from the binary's surroundings: a
    /// checkout's binary builds, an installed one swaps itself in, a named
    /// `$TASKS_REPO` is an explicit request to build, and the bare-name
    /// fallback keeps the behaviour it always had.
    #[test]
    fn a_workspace_above_the_binary_is_what_makes_it_build() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("crates/tasks")).unwrap();
        std::fs::File::create(ws.path().join("crates/tasks/Cargo.toml")).unwrap();
        let in_checkout = ws.path().join("target/debug/tasks");
        assert!(build_in_place(&in_checkout, false));

        let installed = tempfile::tempdir().unwrap();
        let bundled = installed.path().join("Tasks.app/Contents/MacOS/tasks");
        assert!(!build_in_place(&bundled, false));
        assert!(
            build_in_place(&bundled, true),
            "a named repo is a request to build in it"
        );

        assert!(build_in_place(Path::new("tasks"), false));
    }

    /// `cargo` is not on launchd's `PATH`, and `reload`'s first step is a
    /// `cargo build`.
    #[test]
    fn the_childs_path_leads_with_the_toolchain_locations() {
        let path = child_path();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], PathBuf::from("/opt/homebrew/bin"));
        assert_eq!(dirs[1], PathBuf::from("/usr/local/bin"));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(dirs.contains(&PathBuf::from(home).join(".cargo/bin")));
        }
    }

    #[test]
    fn a_run_keeps_only_the_tail_of_a_long_build() {
        let mut run = Run::new(Op::Restart, "tasks reload".into());
        for i in 0..(MAX_LINES + 100) {
            run.push(format!("line {i}"));
        }
        assert_eq!(run.lines.len(), MAX_LINES);
        // The head — including the command line — is what gets dropped.
        assert_eq!(
            run.lines.back().unwrap(),
            &format!("line {}", MAX_LINES + 99)
        );
    }

    #[test]
    fn a_run_is_running_until_it_has_a_verdict() {
        let mut run = Run::new(Op::Stop, "tasks stop".into());
        assert!(run.is_running());
        run.outcome = Some(Outcome::Done);
        assert!(!run.is_running());
    }

    /// The deadlock this module exists to avoid: a child that writes far more
    /// than a pipe buffer holds, on both pipes, then exits with a code that
    /// has to survive the flood.
    #[test]
    fn both_pipes_drain_before_wait() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("noisy.sh");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\n\
             i=0\n\
             while [ $i -lt 5000 ]; do\n\
               echo \"out $i\"\n\
               echo \"err $i\" >&2\n\
               i=$((i+1))\n\
             done\n\
             exit 3"
        )
        .unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let (tx, rx) = mpsc::unbounded();
        pump(Command::new(&script), tx);
        let items: Vec<RunItem> = futures::executor::block_on(rx.collect());

        let mut out = 0;
        let mut err = 0;
        let mut outcome = None;
        for item in &items {
            match item {
                RunItem::Line(line) if line.starts_with("out ") => out += 1,
                RunItem::Line(line) if line.starts_with("err ") => err += 1,
                RunItem::Line(_) => {}
                RunItem::Finished(o) => outcome = Some(*o),
            }
        }
        assert_eq!(out, 5000, "every stdout line arrived");
        assert_eq!(err, 5000, "every stderr line arrived");
        assert_eq!(outcome, Some(Outcome::Busy), "the exit code survived");
        // The verdict is last, always: the window shows it as final.
        assert!(matches!(items.last(), Some(RunItem::Finished(_))));
    }

    #[test]
    fn a_binary_that_will_not_spawn_is_a_verdict_not_a_hang() {
        let (tx, rx) = mpsc::unbounded();
        pump(Command::new("/nonexistent/tasks-binary"), tx);
        let items: Vec<RunItem> = futures::executor::block_on(rx.collect());
        assert!(matches!(
            items.last(),
            Some(RunItem::Finished(Outcome::CouldNotRun))
        ));
    }

    fn write_pidfile(dir: &Path, exe: &Path) {
        let file = tasks_api::paths::PidFile {
            pid: std::process::id(),
            port: 4800,
            started_at: Utc::now(),
            exe: exe.to_path_buf(),
        };
        std::fs::write(
            tasks_api::paths::pid_file(dir),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();
    }
}
