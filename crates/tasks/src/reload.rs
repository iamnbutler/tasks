//! `tasks reload` — build, report, drain, swap, verify.
//!
//! The upgrade loop the server was missing. The order is the whole point:
//!
//! 0. **resolve `TASKS_DEFAULT_MODE`**, because a typo in it makes `serve`
//!    refuse to boot, and discovering that *after* SIGTERMing the old server
//!    turns a typo into an outage. Same rule as "build first";
//! 1. **build** first, so a compile error costs nothing — the old server is
//!    still up and was never signalled;
//! 2. **report** what is in flight, with ages, so "this would kill a scout
//!    that has been running for 20 minutes" is a thing you are told rather
//!    than a thing you discover;
//! 3. **gate** on that report unless told otherwise (`--when-idle`,
//!    `--force`);
//! 4. **swap** with SIGTERM and a real wait for the pid to be gone;
//! 5. **verify** against the *new* pid, and print the migrations that boot
//!    applied.
//!
//! Two facts have to come from the new process rather than be assumed: "did
//! it come up?" and "did the schema move?". Both are answered by `GET
//! /status` on the server that owns the database — which is also why nothing
//! here opens the store. [`Store::open`](crate::store::Store::open) runs
//! migrations, so a supervisor that opened the database would apply the new
//! binary's schema *before* the new binary booted, masking exactly the
//! failure it exists to catch, while the old server is still serving the old
//! schema.
//!
//! A third fact travels the other way. A boot no longer resumes the stored
//! mode ([`crate::run::apply_startup_mode`]), so **an upgrade is the one path
//! that carries it**: [`ModeHandover`] snapshots the old server's mode before
//! the drain and hands it to the child as `TASKS_DEFAULT_MODE`, then verifies
//! against the new pid's `/status` that it came up in it. Everything that is a
//! cold start — a crash loop, `launchd` `KeepAlive`, `tasks serve` by hand —
//! carries nothing and comes back quiet.
//!
//! [`drain_for_maintenance`] and [`resume`] are the same wait loop pointed at
//! a different act. A reload can afford its gate to be a courtesy, because
//! `resume_in_flight` re-attaches to every live VM; the two *host* acts —
//! restarting vm-pool on the same socket, and `make images` — have no such
//! recovery and had no gate at all. `tasks drain` pauses dispatch, waits for
//! the drain point and **keeps holding**, because what happens next is work
//! this process can neither do nor observe.
//!
//! This is not a service manager: no supervision, no restart-on-crash, no
//! daemon. The pidfile is a discovery record and the swap is one-shot;
//! `launchd`/`systemd` compose with it fine, pointed at `tasks serve`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tasks_api::http::{CancelAck, CancelRunRequest, InFlight, ModeResponse, ServerStatus, SetMode};
use tasks_api::models::Mode;
use thiserror::Error;
use tokio::process::Command;

use crate::pidfile::{self, PidFile};

/// How long the old server gets to exit after SIGTERM before SIGKILL.
/// Comfortably past its own 30s in-flight grace, so a graceful drain is never
/// cut short by the supervisor waiting on it.
const STOP_GRACE: Duration = Duration::from_secs(75);

/// How long the new server gets to answer `/status` with its own pid.
const START_TIMEOUT: Duration = Duration::from_secs(90);

/// Gap between `/status` polls while draining. Long enough not to spam the
/// server, short enough that "the last scout finished" is noticed promptly.
const DRAIN_POLL: Duration = Duration::from_secs(3);

/// Default `--drain-timeout`: one scout's own budget (3600s) plus slack, i.e.
/// "wait out at most one scout, then tell me".
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(3900);

/// Per-call budget for the loopback HTTP probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Lines of `serve.log` shown when a boot dies.
const LOG_TAIL_LINES: usize = 20;

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("build failed; the running server was not touched")]
    BuildFailed,
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    DrainTimeout(String),
    #[error("{0}")]
    SwapFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl ReloadError {
    /// Distinct exit codes so a script can branch without parsing prose.
    pub fn exit_code(&self) -> i32 {
        match self {
            ReloadError::Busy(_) => 3,
            ReloadError::DrainTimeout(_) => 4,
            ReloadError::SwapFailed(_) => 5,
            _ => 1,
        }
    }
}

/// What `tasks reload` was asked to do.
#[derive(Debug, Clone)]
pub struct ReloadOptions {
    pub data_dir: PathBuf,
    /// Port for the new server. Defaults to the running server's port, then
    /// to the configured one.
    pub port: Option<u16>,
    /// Build before swapping. `--no-build` turns this off and swaps in
    /// `current_exe()`, which is what makes `make restart` coherent: make
    /// builds, then the freshly built binary swaps *itself* in.
    pub build: bool,
    /// Workspace to build in; detected from the cwd or the running binary
    /// when absent.
    pub repo: Option<PathBuf>,
    /// Wait for destructible work to finish instead of refusing.
    pub when_idle: bool,
    /// How long `--when-idle` waits before giving up.
    pub drain_timeout: Duration,
    /// Swap regardless of what is in flight.
    pub force: bool,
    /// Replace this process with the new server rather than spawning it into
    /// the background — the log in your terminal *is* the verification.
    pub foreground: bool,
}

impl ReloadOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            port: None,
            build: true,
            repo: None,
            when_idle: false,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            force: false,
            foreground: false,
        }
    }
}

/// What this swap owes the pipeline's mode.
///
/// Two separate facts, deliberately not one `Option<Mode>`. `carry` is the
/// mode the *new* server must come up in; `paused_for_drain` records that the
/// drain left the old one paused, which only matters when the swap fails and
/// there is an undo to print. A drain of a `play` server sets both; a
/// `--when-idle` of an already-paused server sets neither.
#[derive(Debug, Clone, Copy, Default)]
struct ModeHandover {
    /// Handed to the child as `TASKS_DEFAULT_MODE`, and verified afterwards.
    /// `None` means "whatever the new server's own configuration says", which
    /// is what every cold start gets.
    carry: Option<Mode>,
    /// The drain paused dispatch. No later boot will unpause it, so a failed
    /// swap has to say so.
    paused_for_drain: bool,
}

impl ModeHandover {
    /// Carry nothing: unknown resolves to quiet, never to dispatching.
    fn none() -> Self {
        Self::default()
    }
}

/// What the caller of [`drain`] owes the mode once the wait succeeds.
///
/// The one place the restart/stop asymmetry is written down. A restart hands
/// the pre-drain mode to the new server, so the pause it installed is undone
/// by the swap; a stop has no successor to hand anything to, and the only slot
/// in which it could write the mode back is *before* the SIGTERM — where
/// unpausing a server that is still running would hand the dispatcher a window
/// to launch one last scout, which is the unattended VM `--when-idle` exists to
/// prevent. So a stop leaves dispatch paused, and says so.
///
/// The third caller is [`drain_for_maintenance`], and it answers the question
/// differently again: **the pause is the deliverable, not the tool**. Nothing
/// follows it at all — the server keeps serving, and what happens next is host
/// work (a vm-pool restart, `make images`) that this process cannot do and
/// cannot observe. So the hold outlives the command and is undone only by
/// [`resume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeAfterDrain {
    /// The swap carries the pre-drain mode to the new server.
    Restored,
    /// Nothing follows, so the pause outlives the command.
    LeftPaused,
    /// The pause *is* the point: it is held until `tasks resume`.
    HeldForMaintenance,
}

impl ModeAfterDrain {
    /// What the pause sentence promises about afterwards.
    fn pause_note(self) -> &'static str {
        match self {
            ModeAfterDrain::Restored => {
                "paused dispatch for the drain (the new server comes up in play)"
            }
            ModeAfterDrain::LeftPaused => {
                "paused dispatch for the drain (it stays paused after the stop)"
            }
            ModeAfterDrain::HeldForMaintenance => {
                "paused dispatch (it stays paused until `tasks resume`)"
            }
        }
    }

    /// What a drain timeout did *not* do. Every variant restores the mode
    /// there — nothing happened, and a no-op must not have side effects.
    ///
    /// The maintenance sentence is imperative because the next thing that
    /// operator does is delete containers: a drain that gave up has quiesced
    /// nothing, and reading "the drain timed out" as "well, near enough" is
    /// exactly the mistake this command exists to prevent.
    fn nothing_happened(self) -> &'static str {
        match self {
            ModeAfterDrain::Restored => "nothing was restarted",
            ModeAfterDrain::LeftPaused => "nothing was stopped",
            ModeAfterDrain::HeldForMaintenance => {
                "the pipeline is not quiesced and dispatch was left as it was — do not \
                 restart vm-pool or rebuild images yet"
            }
        }
    }

    /// What to put on the event feed when the pause goes on, if anything.
    ///
    /// Only the maintenance hold has anything to say there, and it is the one
    /// that needs it: what it leaves behind is a `pause` byte-identical to any
    /// other, so an hour later `/status`, `tasks status` and the Server window
    /// all say `pause` and nothing says why. A restart's pause is undone by
    /// the swap seconds later and a stop's is announced by the stop itself.
    fn feed_note(self) -> Option<&'static str> {
        match self {
            ModeAfterDrain::Restored | ModeAfterDrain::LeftPaused => None,
            ModeAfterDrain::HeldForMaintenance => Some(
                "`tasks drain`: dispatch is held for host maintenance (a vm-pool restart or \
                 `make images`); `tasks resume` releases it",
            ),
        }
    }

    /// …and when a timeout puts it back. Same asymmetry, same reason: this is
    /// the edge that would otherwise leave a reader guessing.
    fn restore_note(self) -> Option<&'static str> {
        match self {
            ModeAfterDrain::Restored | ModeAfterDrain::LeftPaused => None,
            ModeAfterDrain::HeldForMaintenance => Some(
                "`tasks drain` timed out before the pipeline was quiesced; dispatch is back \
                 to play and nothing is held",
            ),
        }
    }
}

/// What `tasks stop` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct StopOptions {
    /// Wait for destructible work to finish before signalling anything.
    /// Plain `tasks stop` is unchanged: immediate and ungated, because it is
    /// the counterpart of `reload --force` and the documented way through
    /// every refusal below.
    pub when_idle: bool,
    /// How long `--when-idle` waits before giving up.
    pub drain_timeout: Duration,
}

impl Default for StopOptions {
    fn default() -> Self {
        Self {
            when_idle: false,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
        }
    }
}

/// What a stop stopped, and the one lasting consequence it had.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub file: PidFile,
    /// The drain paused dispatch and nothing will undo it — a boot takes its
    /// configured default, so the next server does not resume it either.
    pub left_paused: bool,
}

/// Build, report, gate, drain, swap, verify. See the module docs for why in
/// that order.
pub async fn reload(opts: ReloadOptions) -> Result<(), ReloadError> {
    // 0. Resolve the mode the replacement would boot into. Before the build
    //    and before anything is signalled, because an unusable
    //    TASKS_DEFAULT_MODE is a hard `serve` startup error: finding that out
    //    after the SIGTERM would leave nothing serving at all.
    let default_mode = crate::run::startup_mode_from_env()
        .map_err(|err| ReloadError::Other(format!("{err}; nothing was touched")))?;

    // 1. Build: a failure here must cost nothing, and it can only cost
    //    nothing while the old server has not been signalled.
    let binary = match opts.build {
        true => build(opts.repo.as_deref()).await?,
        false => std::env::current_exe()?,
    };

    // 2. Report. Both halves are hints until proven: the pidfile is a
    //    discovery record, and liveness comes from the OS and from an answer
    //    on the port, never from the file.
    let existing = pidfile::read(&opts.data_dir).filter(|f| pidfile::pid_alive(f.pid));
    let status = match &existing {
        Some(file) => fetch_status(file.port).await.ok(),
        None => None,
    };
    print!(
        "{}",
        render_status(existing.as_ref(), status.as_ref(), Utc::now())
    );

    let port = opts
        .port
        .or(existing.as_ref().map(|f| f.port))
        .unwrap_or(crate::run::DEFAULT_PORT);

    // Delegation: when the serving binary is the launchd service's (or the
    // service is installed and nothing serves), launchd owns the lifecycle.
    // Under `KeepAlive` a bare SIGTERM is a *restart* — this path would
    // report a swap while launchd resurrects the old server behind it — so
    // the swap becomes: put the binary in the service's home, kickstart, and
    // verify. A pidfile naming any other binary is a developer serving
    // beside the service, and their reload keeps meaning what it always did.
    let managed = crate::service::managed(&opts.data_dir);
    if managed.is_some() && opts.foreground && existing.is_some() {
        return Err(ReloadError::Other(format!(
            "this server is launchd-managed ({}); --foreground would race the \
             service for the port. `tasks service stop` first, or drop --foreground",
            crate::service::LABEL
        )));
    }

    let Some(existing) = existing else {
        // Nothing is running: a reload is just a start, and a start is a cold
        // start — there is no mode to carry from anywhere. With a service
        // installed the start *is* the service's (`--foreground` excepted:
        // an exec into this terminal touches no agent, and the pidfile it
        // writes names a non-service binary, so nothing later mistakes it
        // for the service).
        return match (&managed, opts.foreground) {
            (Some(paths), false) => {
                managed_swap(&binary, &opts, paths, None, ModeHandover::none()).await
            }
            _ => start(&binary, port, &opts, ModeHandover::none()).await,
        };
    };

    // 3. Gate. A live pid that will not answer may be mid-shutdown; killing
    //    it blind is the caller's call, not ours.
    let Some(status) = status else {
        if !opts.force {
            return Err(ReloadError::SwapFailed(format!(
                "pid {} is alive but is not answering /status on port {}; \
                 it may be shutting down. Re-run, or `tasks reload --force` to \
                 SIGTERM it anyway",
                existing.pid, existing.port
            )));
        }
        // A server too wedged to answer `/status` never told us its mode, and
        // guessing `play` at a machine nobody is watching is the wrong way to
        // be wrong.
        return match &managed {
            Some(paths) => {
                managed_swap(
                    &binary,
                    &opts,
                    paths,
                    Some(existing.pid),
                    ModeHandover::none(),
                )
                .await
            }
            None => swap(&binary, &existing, port, &opts, ModeHandover::none()).await,
        };
    };

    // The mode to carry is read *now*, before the drain: `--when-idle` writes
    // `pause` to make the wait terminate, so reading it afterwards would carry
    // the tool instead of the intent. Nothing is carried when it matches what
    // the new server would take anyway — a carry is an override, and an
    // override that agrees with the default is just noise in the output.
    let mut handover = ModeHandover {
        carry: (status.mode != default_mode).then_some(status.mode),
        paused_for_drain: false,
    };

    if status.in_flight.is_destructible() && !opts.force {
        if !opts.when_idle {
            return Err(ReloadError::Busy(format!(
                "{} in flight; a restart would destroy it. \
                 `--when-idle` waits for a drain point, `--force` swaps anyway",
                describe(&status.in_flight)
            )));
        }
        // 4. Drain. Pausing is load-bearing, not politeness: with a non-empty
        //    queue the dispatcher starts a fresh scout the moment one
        //    finishes, so a wait that did not pause would never terminate.
        handover.paused_for_drain = drain(
            existing.port,
            status.mode,
            opts.drain_timeout,
            ModeAfterDrain::Restored,
        )
        .await?;
    }

    if status.in_flight.orchestrator.is_some() {
        println!(
            "note: an orchestrator turn is owed; the restart costs it one turn \
             (the next boot takes it again)"
        );
    }

    match &managed {
        Some(paths) => managed_swap(&binary, &opts, paths, Some(existing.pid), handover).await,
        None => swap(&binary, &existing, port, &opts, handover).await,
    }
}

/// The managed half of a swap: make the launchd service serve `binary`.
///
/// Install into the service's home (write-then-rename; a no-op when the
/// binary already is the home's), kickstart, and verify against the *new*
/// pid — launchd reports nothing when it relaunches, so the proof comes from
/// `/status`, the same two facts the unmanaged swap proves.
///
/// The mode carry is the one part that degrades: an unmanaged start hands
/// the mode over in the child's environment, but launchd owns this child's
/// environment, and pinning a carried mode into the plist would change what
/// a *crash* restart boots into. So the carry is a `POST /mode` after the
/// verify, and the window between boot and that write runs in the plist's
/// default — quiet, unless the operator pinned `--default-mode play`, which
/// is why the carry names the window when it happens.
async fn managed_swap(
    binary: &Path,
    opts: &ReloadOptions,
    paths: &crate::service::ServicePaths,
    previous: Option<u32>,
    handover: ModeHandover,
) -> Result<(), ReloadError> {
    let to_err = |e: crate::service::ServiceError| ReloadError::Other(e.to_string());

    if crate::service::install_binary(binary, paths).map_err(to_err)? {
        println!("installed {} -> {}", binary.display(), paths.bin.display());
    }
    println!(
        "restarting the launchd service ({})…",
        crate::service::LABEL
    );
    match crate::service::loaded().await.map_err(to_err)? {
        true => crate::service::launchctl_kickstart(true)
            .await
            .map_err(to_err)?,
        false => crate::service::bootstrap(paths).await.map_err(to_err)?,
    }
    let file = crate::service::wait_for_serving(&opts.data_dir, previous)
        .await
        .map_err(|e| ReloadError::SwapFailed(e.to_string()))?;
    println!("serving: pid {} on port {}", file.pid, file.port);
    if let Ok(status) = fetch_status(file.port).await {
        print!("{}", render_migrations(&status));
    }
    if let Some(mode) = handover.carry {
        match set_mode(file.port, mode, Some("mode carried over a managed restart")).await {
            Ok(()) => println!(
                "mode carried: {} (the boot itself ran in the agent's default until this write)",
                mode.as_str()
            ),
            Err(err) => {
                // The same lasting consequence swap() names: the drain may
                // have paused a pipeline the carry was about to unpause.
                println!(
                    "could not carry the mode ({err}); set it by hand: \
                     curl -sS -X POST localhost:{}/mode -H 'content-type: application/json' \
                     -d '{{\"mode\":\"{}\"}}'",
                    file.port,
                    mode.as_str()
                );
            }
        }
    }
    Ok(())
}

/// SIGTERM the old server, start the new one, verify, carry the mode over.
async fn swap(
    binary: &Path,
    existing: &PidFile,
    port: u16,
    opts: &ReloadOptions,
    handover: ModeHandover,
) -> Result<(), ReloadError> {
    println!("stopping pid {}…", existing.pid);
    stop_pid(existing.pid).await?;
    println!("stopped");
    match start(binary, port, opts, handover).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if handover.paused_for_drain {
                // The drain paused the pipeline, the server that could unpause
                // it did not come up, and no later boot will do it either —
                // a boot takes its configured default, not the stored mode.
                // Say so, with the undo.
                println!(
                    "the pipeline is still paused; once a server is up: \
                     curl -sS -X POST localhost:{port}/mode -H 'content-type: application/json' \
                     -d '{{\"mode\":\"play\"}}'"
                );
            }
            Err(err)
        }
    }
}

/// Launch the new server and prove it is the one answering.
///
/// The mode travels in the child's *environment*, never as a `POST /mode`
/// after the boot. Three reasons, any one of them sufficient: a POST leaves a
/// window in which the new server is already running in its configured default
/// (with `TASKS_DEFAULT_MODE=play` and a paused old server, that window
/// dispatches); `--foreground` execs, so there is no "later" in which to
/// restore anything; and the real environment outranks every `.env`, which is
/// exactly the precedence an explicit upgrade wants.
async fn start(
    binary: &Path,
    port: u16,
    opts: &ReloadOptions,
    handover: ModeHandover,
) -> Result<(), ReloadError> {
    if opts.foreground {
        // exec, so the log in this terminal is the server's own and ctrl-c
        // reaches it directly. Nothing after this line runs — which is fine
        // for the mode, because it is passed in the environment rather than
        // applied afterwards.
        return Err(exec_foreground(
            binary,
            port,
            &opts.data_dir,
            handover.carry,
        ));
    }

    let log_path = tasks_api::paths::serve_log(&opts.data_dir);
    tokio::fs::create_dir_all(&opts.data_dir).await?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut command = Command::new(binary);
    command
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .env("TASKS_DATA_DIR", &opts.data_dir)
        .current_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .process_group(0);
    if let Some(mode) = handover.carry {
        command.env("TASKS_DEFAULT_MODE", mode.as_str());
    }
    let mut child = command.spawn()?;
    let child_pid = child.id().unwrap_or(0);

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let status = loop {
        // A boot that dies — a migration that will not apply, a port already
        // taken — is reported at once rather than waited out.
        if let Some(exit) = child.try_wait()? {
            return Err(ReloadError::SwapFailed(format!(
                "the new server exited immediately ({exit}); tail of {}:\n{}",
                log_path.display(),
                log_tail(&log_path)
            )));
        }
        // Verify the *pid*, not the port: "something answers /status" is also
        // satisfied by a stale listener or by a server someone else started.
        if let Ok(status) = fetch_status(port).await
            && status.pid == child_pid
        {
            break status;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ReloadError::SwapFailed(format!(
                "pid {child_pid} did not answer /status on port {port} within {}s; tail of {}:\n{}",
                START_TIMEOUT.as_secs(),
                log_path.display(),
                log_tail(&log_path)
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    println!(
        "up: pid {} port {port}, logging to {}",
        status.pid,
        log_path.display()
    );
    println!("{}", render_migrations(&status));

    // The carry is verified, not assumed — same posture as the migration
    // check, and for the same reason: it is a claim about the new process that
    // only the new process can settle. A `.env` that sets `TASKS_DEFAULT_MODE`
    // cannot beat the environment we spawned with, but an old binary that does
    // not read the variable at all can, and that is worth being told about.
    if let Some(mode) = handover.carry {
        if status.mode == mode {
            println!("mode carried over: {}", mode.as_str());
        } else {
            println!(
                "mode did not carry over: asked for {}, came up in {}; \
                 curl -sS -X POST localhost:{port}/mode -H 'content-type: application/json' \
                 -d '{{\"mode\":\"{}\"}}'",
                mode.as_str(),
                status.mode.as_str(),
                mode.as_str()
            );
        }
    }
    Ok(())
}

/// Replace this process with `tasks serve`. Only returns on failure.
fn exec_foreground(binary: &Path, port: u16, data_dir: &Path, carry: Option<Mode>) -> ReloadError {
    use std::os::unix::process::CommandExt;
    println!("exec {} serve --port {port}", binary.display());
    let mut command = std::process::Command::new(binary);
    command
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .env("TASKS_DATA_DIR", data_dir);
    if let Some(mode) = carry {
        println!("carrying the mode over: {}", mode.as_str());
        command.env("TASKS_DEFAULT_MODE", mode.as_str());
    }
    ReloadError::Io(command.exec())
}

/// Pause dispatch, wait for destructible work to finish, and report whether
/// this pause is still outstanding (`false` when the pipeline was not playing
/// to begin with, so there was nothing to pause).
///
/// The pause is the tool and not the intent, which is why the mode the swap
/// carries is snapshotted by the caller *before* this runs. What happens to it
/// afterwards is [`ModeAfterDrain`], and every caller has to answer it: a
/// restart hands it to the new server, a stop leaves it paused.
async fn drain(
    port: u16,
    mode: Mode,
    timeout: Duration,
    after: ModeAfterDrain,
) -> Result<bool, ReloadError> {
    let paused = pause_dispatch(port, mode, after).await?;
    wait_for_drain_point(port, timeout, paused, after).await
}

/// Hold new dispatch, and report whether *this* call is what installed the
/// hold (`false` when the pipeline was not playing to begin with).
///
/// Split out of [`drain`] so [`drain_for_maintenance`] can put its cancels in
/// the gap between the pause and the wait — cancelling first would let the
/// dispatcher start a replacement scout within the tick, the same reason a
/// cancelled scout's task returns to `backlog` rather than `queued`. Splitting
/// rather than copying is what keeps one pause rule and one wait loop in the
/// binary.
///
/// A mode that is not `Play` is left exactly as it is, never rewritten to
/// `Pause`: `Stop` is *tighter* than `Pause` (it stops the poller too), so
/// "pausing" a stopped pipeline would quietly turn intake back on in the name
/// of holding it. What the maintenance hold still does there is *say so* on
/// the feed, which is why the note travels with the mode we already have.
async fn pause_dispatch(port: u16, mode: Mode, after: ModeAfterDrain) -> Result<bool, ReloadError> {
    let paused = mode == Mode::Play;
    let target = match paused {
        true => Mode::Pause,
        false => mode,
    };
    if paused || after.feed_note().is_some() {
        set_mode(port, target, after.feed_note())
            .await
            .map_err(ReloadError::Other)?;
    }
    match paused {
        true => println!("{}", after.pause_note()),
        // Already not dispatching; nothing to pause and nothing to undo.
        false => println!("mode is {}; nothing new will be dispatched", mode.as_str()),
    }
    Ok(paused)
}

/// Wait for the last destructible run to land, and answer with the `paused`
/// it was handed — the value a caller has to know afterwards, and the one the
/// timeout path has to undo.
async fn wait_for_drain_point(
    port: u16,
    timeout: Duration,
    paused: bool,
    after: ModeAfterDrain,
) -> Result<bool, ReloadError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    loop {
        match fetch_status(port).await {
            Ok(status) if !status.in_flight.is_destructible() => {
                println!("drained");
                return Ok(paused);
            }
            Ok(status) => {
                let line = describe(&status.in_flight);
                if line != last {
                    println!("waiting: {line}");
                    last = line;
                }
            }
            Err(err) => {
                // The server we are waiting on went away; nothing left to
                // drain, and the swap will find no live pid.
                println!("waiting: {err}");
            }
        }
        if tokio::time::Instant::now() >= deadline {
            // Restore immediately, whatever the caller wanted afterwards:
            // this restarted and stopped nothing, so leaving the pipeline
            // paused would be a side effect of a no-op. It has to happen here
            // and not at the next boot — a boot takes its configured default
            // now, so nothing else would ever undo it.
            if paused && let Err(err) = set_mode(port, Mode::Play, after.restore_note()).await {
                println!("could not restore mode to play: {err}");
            }
            return Err(ReloadError::DrainTimeout(format!(
                "still busy after {}s; {}",
                timeout.as_secs(),
                after.nothing_happened()
            )));
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

/// SIGTERM `pid` and wait until it is really gone, SIGKILL as a last resort.
///
/// Shared by `reload` and `stop`, so there is exactly one implementation of
/// "wait until it is actually gone" — and one place that knows a zombie is
/// gone (see [`pidfile::pid_alive`]).
pub async fn stop_pid(pid: u32) -> Result<(), ReloadError> {
    signal(pid, "TERM").await;
    if wait_gone(pid, STOP_GRACE).await {
        return Ok(());
    }
    println!(
        "pid {pid} still alive after {}s; sending SIGKILL",
        STOP_GRACE.as_secs()
    );
    signal(pid, "KILL").await;
    match wait_gone(pid, Duration::from_secs(10)).await {
        true => Ok(()),
        false => Err(ReloadError::SwapFailed(format!(
            "pid {pid} would not stop, even after SIGKILL"
        ))),
    }
}

/// `tasks stop`: shut down the running server, if there is one. Returns what
/// it stopped.
///
/// `--when-idle` waits for a drain point first, on the *same*
/// [`InFlight::is_destructible`] predicate `reload --when-idle` waits on — so
/// Restart When Idle and Stop When Idle cannot disagree about what idle means.
/// It differs in what it leaves behind: there is no successor to hand the mode
/// to, so dispatch stays paused (see [`ModeAfterDrain`]).
pub async fn stop(data_dir: &Path, opts: StopOptions) -> Result<Option<Stopped>, ReloadError> {
    let Some(file) = pidfile::read_live(data_dir) else {
        return Ok(None);
    };
    let left_paused = match opts.when_idle {
        true => wait_for_idle(&file, opts.drain_timeout).await?,
        false => false,
    };
    // A launchd-managed server cannot be stopped by SIGTERM — `KeepAlive`
    // turns that into a restart, and this function would report "stopped"
    // while launchd resurrects the server behind the report. Unloading the
    // job is the stop that sticks. `managed` already requires the pidfile to
    // name the service's own binary, so a developer's server beside the
    // service still takes the signal path below.
    if crate::service::managed(data_dir).is_some() {
        println!(
            "this server is launchd-managed ({}); unloading the agent",
            crate::service::LABEL
        );
        match crate::service::bootout().await {
            Ok(()) => {
                if !wait_gone(file.pid, STOP_GRACE).await {
                    return Err(ReloadError::SwapFailed(format!(
                        "the agent was unloaded but pid {} is still running",
                        file.pid
                    )));
                }
                pidfile::remove_if_ours(data_dir, file.pid);
                println!(
                    "the agent stays unloaded until `tasks service start` or the next \
                     login; `tasks service uninstall` is the durable off"
                );
                return Ok(Some(Stopped { file, left_paused }));
            }
            // A serving pid from the service's binary with no loaded job —
            // someone exec'd it in a terminal. The signal path below is then
            // both safe (nothing will resurrect it) and the only stop there
            // is.
            Err(err) => println!("could not unload the agent ({err}); stopping the pid directly"),
        }
    }
    stop_pid(file.pid).await?;
    // Belt and braces: the server clears its own record on the way out, but a
    // SIGKILLed one cannot.
    pidfile::remove_if_ours(data_dir, file.pid);
    Ok(Some(Stopped { file, left_paused }))
}

/// Report what is in flight and wait for it to land. Answers "was dispatch
/// left paused".
///
/// A live pid that will not answer `/status` cannot be waited on at all —
/// there is no way to tell when idle arrives — so this refuses (exit 3) with
/// the server untouched and names plain `tasks stop` as the way through. That
/// is the only refusal here: an idle server returns before touching the mode,
/// because a wait that never happened must not leave a lasting side effect.
async fn wait_for_idle(file: &PidFile, timeout: Duration) -> Result<bool, ReloadError> {
    let status = fetch_status(file.port).await.ok();
    print!("{}", render_status(Some(file), status.as_ref(), Utc::now()));

    let Some(status) = status else {
        return Err(ReloadError::Busy(format!(
            "pid {} is alive but is not answering /status on port {}, so there is no \
             way to tell when it is idle; `tasks stop` stops it now",
            file.pid, file.port
        )));
    };

    // Reported, never waited for — same reasoning as `reload`: the answered
    // watermark means a stop mid-turn costs one turn, and nothing else can
    // pick a turn up anyway.
    if status.in_flight.orchestrator.is_some() {
        println!(
            "note: an orchestrator turn is owed; the stop costs it one turn \
             (the next boot takes it again)"
        );
    }

    if !status.in_flight.is_destructible() {
        println!("nothing in flight; stopping now");
        return Ok(false);
    }

    drain(file.port, status.mode, timeout, ModeAfterDrain::LeftPaused).await
}

/// The undo for the pause a `--when-idle` stop leaves behind. Printed last, so
/// the lasting consequence is the last thing said.
pub fn render_left_paused(port: u16) -> String {
    format!(
        "dispatch is left paused, and no boot will resume it. Once a server is up: \
         curl -sS -X POST localhost:{port}/mode -H 'content-type: application/json' \
         -d '{{\"mode\":\"play\"}}'"
    )
}

// --- the maintenance drain ---

/// How long a cancel gets to answer. Longer than [`PROBE_TIMEOUT`] on purpose:
/// the handler waits out its own settle window before replying, so the honest
/// budget here is that window plus the round trip.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(15);

/// What `tasks drain` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct DrainOptions {
    /// Report whether the pipeline is quiesced and exit. Touches neither the
    /// mode nor any run — this is what `make images` gates on.
    pub check: bool,
    /// Cancel running scouts rather than waiting them out. Strictly opt-in,
    /// and never the default: waiting costs time, cancelling costs work.
    pub cancel_scouts: bool,
    /// How long to wait for the drain point before giving up.
    pub drain_timeout: Duration,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            check: false,
            cancel_scouts: false,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
        }
    }
}

/// What a drain found, and what it left behind.
///
/// Three variants and not two. [`Drained::Clear`] is `--check` finding nothing
/// to wait for, and it renders **empty**: `Quiesced { left_paused: false }`'s
/// sentence is about a pipeline this command is holding, and a check holds
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// No live server. Nothing here can start or watch a container, so there
    /// is nothing to hold — and the gate has to pass, or it would only ever
    /// work on a host that happens to be serving.
    NotServing,
    /// `--check`: nothing in flight and dispatch is not playing.
    Clear,
    /// The pipeline is quiesced and dispatch is held until [`resume`].
    Quiesced { port: u16, left_paused: bool },
}

/// `tasks drain`: pause dispatch, wait for in-flight work to land, and **keep
/// holding** — so the operator can restart vm-pool or rebuild the images.
///
/// The half of #961 neither the update hold nor `POST /runs/cancel-all`
/// covers. `tasks reload` needs no gate (`resume_in_flight` re-attaches to
/// every live VM); the two *host* actions have no such recovery and had no
/// gate at all.
///
/// Three things it deliberately is not. It never signals the server — the API
/// keeps serving throughout, which is what makes it usable *before* a pool
/// restart. It installs no new server-side hold: mode `pause` is the hold,
/// already read by all three places that select work, and #961 §2 says extend
/// the existing drain rather than stand a parallel one beside it. And nothing
/// resumes automatically, because only the operator knows the pool is back and
/// the images are rebuilt.
pub async fn drain_for_maintenance(
    data_dir: &Path,
    opts: DrainOptions,
) -> Result<Drained, ReloadError> {
    let Some(file) = pidfile::read_live(data_dir) else {
        // Not a refusal: no dispatcher means nothing that can start a
        // container, and a gate that failed here would only work on a host
        // already serving.
        println!("not serving — nothing here can start or watch a container");
        return Ok(Drained::NotServing);
    };

    let status = fetch_status(file.port).await.ok();
    print!(
        "{}",
        render_status(Some(&file), status.as_ref(), Utc::now())
    );

    // The one refusal that is not about the pipeline's state: there is no way
    // to tell when idle arrives, and "quiesced" about a server we cannot see
    // into is the wrong direction to be wrong in.
    let Some(status) = status else {
        return Err(ReloadError::Busy(format!(
            "pid {} is alive but is not answering /status on port {}, so there is no way to \
             tell what is in flight. Do not restart vm-pool or rebuild images yet; re-run \
             once it answers, or `tasks stop` stops it",
            file.pid, file.port
        )));
    };

    if opts.check {
        return check(&status).map(|()| Drained::Clear);
    }

    // Reported and never waited for: an orchestrator turn is a local child, no
    // VM holds it, and neither host action costs it anything.
    if status.in_flight.orchestrator.is_some() {
        println!("note: an orchestrator turn is owed; no VM holds it, so it is not waited for");
    }

    // Unconditionally, even with nothing in flight — the deliberate inversion
    // of `stop --when-idle`, which returns early without touching the mode. An
    // idle pipeline nobody holds starts a scout on the next tick, straight
    // into the pool that is about to go down.
    let paused = pause_dispatch(file.port, status.mode, ModeAfterDrain::HeldForMaintenance).await?;

    if opts.cancel_scouts {
        cancel_running_scouts(file.port).await;
    }

    wait_for_drain_point(
        file.port,
        opts.drain_timeout,
        paused,
        ModeAfterDrain::HeldForMaintenance,
    )
    .await?;

    Ok(Drained::Quiesced {
        port: file.port,
        left_paused: paused,
    })
}

/// `--check`: is this host safe to do the destructive work on *right now*?
///
/// Two conditions, and the second is the one that is easy to miss. Nothing in
/// flight is not enough: a *playing* pipeline with nothing in flight tops
/// scouts up on the dispatcher's next tick, so a multi-minute image rebuild
/// started here races it — and a scout that starts during a rebuild starts in
/// the **old** image, which is the #909 staleness [`crate::updates`] exists to
/// prevent and cannot see, since the identity it reads is only ever observed
/// from a run that has already started. So a playing server is refused with
/// `tasks drain` named, and `FORCE=1` remains the escape hatch for someone who
/// knows better.
fn check(status: &ServerStatus) -> Result<(), ReloadError> {
    if status.in_flight.is_destructible() {
        return Err(ReloadError::Busy(format!(
            "{} in flight — restarting vm-pool or rebuilding images now would land on it. \
             `tasks drain` waits it out and holds the pipeline",
            describe(&status.in_flight)
        )));
    }
    if status.mode == Mode::Play {
        return Err(ReloadError::Busy(
            "nothing is in flight, but dispatch is playing: the dispatcher tops scouts up on \
             its next tick, and a scout that starts during a rebuild starts in the old image. \
             `tasks drain` holds the pipeline, `tasks resume` releases it"
                .into(),
        ));
    }
    println!(
        "quiesced: nothing in flight, dispatch is {}",
        status.mode.as_str()
    );
    Ok(())
}

/// What a held pipeline leaves the operator holding, and how to give it back.
///
/// Pure and rendered from the outcome rather than printed as it goes, so both
/// arms are unit-testable — and so `--check`, which holds nothing, cannot
/// borrow the held drain's closing words.
pub fn render_quiesced(drained: &Drained) -> String {
    let Drained::Quiesced { port, left_paused } = drained else {
        return String::new();
    };
    let held = match left_paused {
        true => "dispatch is paused and stays paused",
        // Not "dispatch was already not playing" for a `Clear` check — see
        // `Drained`. Here it is true and it is the reader's next question.
        false => "dispatch was already not playing, and this changed nothing",
    };
    format!(
        "quiesced: nothing is in flight and {held}. vm-pool can be restarted and \
         `make images` run now.\n\
         when the host work is done: tasks resume   (or curl -sS -X POST \
         localhost:{port}/mode -H 'content-type: application/json' -d '{{\"mode\":\"play\"}}')"
    )
}

/// Ask the server to stop every running scout, and report what it answered.
///
/// **A cancel does not guarantee the drain point arrives.** It writes a durable
/// `cancellations` row and the *dispatcher following the run* is what concludes
/// it, so a run nothing is following — its dispatcher died, or vm-pool is
/// already down — still runs the wait out to the timeout. That is the honest
/// answer rather than a gap: this command promises the pipeline is quiesced,
/// and it cannot promise that about a VM nobody is watching. Which is why each
/// line below repeats the server's own `concluded` instead of flattening
/// "asked" and "stopped" into one word.
///
/// Scouts only, and never builds: a build is one serial lane and a whole
/// implementation, and #961 §3's own accounting (3 scouts, 0 strikes, ~30 min)
/// argues waiting beats discarding. Routed through the API rather than by
/// removing the VM, because a cancel has to interrupt the dispatcher's drain
/// (#876) — killing the container by hand leaves the row `running` forever.
async fn cancel_running_scouts(port: u16) {
    // Re-read *after* the pause: the set worth cancelling is the one running
    // now, not the one that was running before dispatch was held.
    let scouts = match fetch_status(port).await {
        Ok(status) => status.in_flight.scouts,
        Err(err) => {
            println!("could not re-read what is in flight ({err}); no scout was cancelled");
            return;
        }
    };
    if scouts.is_empty() {
        println!("no running scout to cancel");
        return;
    }
    for scout in &scouts {
        match cancel_scout(port, &scout.id).await {
            Ok(ack) => match ack.concluded {
                true => println!("cancelled scout {}: it is now {}", ack.run_id, ack.status),
                false => println!(
                    "asked scout {} to stop; it is still {} — whoever is following the run \
                     concludes it, and this drain waits for that",
                    ack.run_id, ack.status
                ),
            },
            Err(err) => println!("could not cancel scout {}: {err}", scout.id),
        }
    }
}

/// One `POST /sessions/{id}/cancel`.
///
/// The rationale is not decoration: it lands in the run's `exit_reason`, which
/// is what tells a reader months later that this was a deliberate stop and not
/// a crash.
async fn cancel_scout(port: u16, id: &str) -> Result<CancelAck, String> {
    let client = reqwest::Client::builder()
        .timeout(CANCEL_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/sessions/{id}/cancel"))
        .json(&CancelRunRequest {
            rationale: Some(
                "`tasks drain --cancel-scouts`: host maintenance (a vm-pool restart or an \
                 image rebuild) is about to happen and this run was not waited out"
                    .into(),
            ),
            evidence: None,
        })
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("/cancel answered {}", response.status()));
    }
    response
        .json::<CancelAck>()
        .await
        .map_err(|e| format!("could not read the cancel answer: {e}"))
}

/// `tasks resume`: the undo for [`drain_for_maintenance`]. Returns the port
/// and the mode it found, or `None` when nothing is serving.
///
/// It reports what it changed *from* rather than assuming `pause`: a drain of
/// a stopped pipeline holds it without rewriting the mode, so resuming one is
/// a promotion, and saying so is the least this owes the operator.
pub async fn resume(data_dir: &Path) -> Result<Option<(u16, Mode)>, ReloadError> {
    let Some(file) = pidfile::read_live(data_dir) else {
        return Ok(None);
    };
    let status = fetch_status(file.port).await.map_err(ReloadError::Other)?;
    set_mode(
        file.port,
        Mode::Play,
        Some("`tasks resume`: the maintenance hold is released and dispatch is playing"),
    )
    .await
    .map_err(ReloadError::Other)?;
    Ok(Some((file.port, status.mode)))
}

/// `tasks status`: the same report `reload` prints, on its own.
pub async fn report(data_dir: &Path) -> (String, bool) {
    let file = pidfile::read(data_dir).filter(|f| pidfile::pid_alive(f.pid));
    let status = match &file {
        Some(file) => fetch_status(file.port).await.ok(),
        None => None,
    };
    let serving = status.is_some();
    (
        render_status(file.as_ref(), status.as_ref(), Utc::now()),
        serving,
    )
}

async fn signal(pid: u32, sig: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

pub(crate) async fn wait_gone(pid: u32, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if !pidfile::pid_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// --- build ---

/// `cargo build -p tasks`, returning the artifact it produced.
///
/// `json-render-diagnostics` gives us the artifact path on stdout while cargo
/// still renders errors on stderr the way it always does — a failed build
/// should look like a failed build.
async fn build(repo: Option<&Path>) -> Result<PathBuf, ReloadError> {
    let workspace = match repo {
        Some(path) => path.to_path_buf(),
        None => find_workspace().ok_or_else(|| {
            ReloadError::Other(
                "could not find the tasks workspace; run from a checkout or pass --repo PATH"
                    .into(),
            )
        })?,
    };
    println!("building in {}…", workspace.display());
    let output = Command::new("cargo")
        .args([
            "build",
            "-p",
            "tasks",
            "--message-format",
            "json-render-diagnostics",
        ])
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await?;
    if !output.status.success() {
        return Err(ReloadError::BuildFailed);
    }
    let binary = artifact_path(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| ReloadError::Other("cargo reported no `tasks` executable".into()))?;
    println!("built {}", binary.display());
    Ok(binary)
}

/// The `tasks` executable out of cargo's JSON message stream.
fn artifact_path(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact"))
        .filter(|msg| {
            msg.get("target")
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                == Some("tasks")
        })
        .filter_map(|msg| Some(PathBuf::from(msg.get("executable")?.as_str()?)))
        .next_back()
}

/// The workspace root: from the cwd if we are standing in a checkout, else
/// from the running binary's ancestors (`…/target/debug/tasks`).
fn find_workspace() -> Option<PathBuf> {
    let from_cwd = std::env::current_dir()
        .ok()
        .and_then(|d| workspace_above(&d));
    from_cwd.or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| workspace_above(&exe))
    })
}

/// `pub(crate)` because it is also the probe behind the vm-pool autospawn
/// default ([`crate::run`]): "is this binary a checkout artifact" and "is
/// there a workspace to build in" must stay one question, or the app could
/// drive a binary with `--no-build` that the server half still treats as a
/// developer's.
pub(crate) fn workspace_above(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("crates/tasks/Cargo.toml").is_file())
        .map(PathBuf::from)
}

// --- probes ---

pub(crate) async fn fetch_status(port: u16) -> Result<ServerStatus, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("http://127.0.0.1:{port}/status"))
        .send()
        .await
        .map_err(|e| format!("no answer on port {port}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("/status answered {}", response.status()));
    }
    response
        .json::<ServerStatus>()
        .await
        .map_err(|e| format!("could not read /status: {e}"))
}

/// `POST /mode`, with the reason for it when there is one.
///
/// The note is what the server puts on the event feed; the mode is the
/// standing answer. There is deliberately nothing between them — a persisted
/// "held for maintenance" flag would be a fourth hold to keep in step with
/// `github_hold` and `update_hold`, for a fact the feed and the mode already
/// carry between them.
async fn set_mode(port: u16, mode: Mode, note: Option<&str>) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/mode"))
        .json(&SetMode {
            mode: mode.as_str().to_string(),
            note: note.map(str::to_string),
        })
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("/mode answered {}", response.status()));
    }
    response
        .json::<ModeResponse>()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// --- rendering ---

/// The whole report as text, from the two things we know: what the pidfile
/// claims and what the server answered. Pure, so every path is unit testable
/// without a process — which is most of what makes this tool trustworthy.
pub fn render_status(
    file: Option<&PidFile>,
    status: Option<&ServerStatus>,
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    match (file, status) {
        (None, _) => {
            out.push_str("not serving (no pidfile)\n");
        }
        (Some(file), None) => {
            out.push_str(&format!(
                "pid {} is alive (port {}, {}) but is not answering /status\n",
                file.pid,
                file.port,
                file.exe.display()
            ));
        }
        (Some(file), Some(status)) => {
            out.push_str(&format!(
                "serving  pid {}  port {}  up {}\n",
                status.pid,
                file.port,
                humanize(now - status.started_at)
            ));
            out.push_str(&format!("binary   {}\n", file.exe.display()));
            out.push_str(&format!("mode     {}\n", status.mode.as_str()));
            out.push_str(&render_github_hold(status, now));
            out.push_str(&render_update_pending(status));
            out.push_str(&render_pool_hold(status, now));
            out.push_str(&render_verify_dir(status, now));
            out.push_str(&render_images(status));
            out.push_str(&render_in_flight(&status.in_flight, now));
        }
    }
    out
}

/// Why the pipeline is idle, when the reason is that GitHub is not answering.
///
/// **Silent when there is no hold.** A standing "GitHub ok" line is one a
/// reader learns to skip, and this one has to land the one time it appears.
///
/// It prints two ages, because they answer different questions: how long the
/// outage has run, and how long ago the last observation was — the gap between
/// them is the difference between a hold somebody is still refreshing and one
/// about to expire on its own. And it says what holding *costs*, because the
/// reader's next question is whether the pipeline is losing work.
pub fn render_github_hold(status: &ServerStatus, now: DateTime<Utc>) -> String {
    let Some(hold) = &status.github else {
        return String::new();
    };
    format!(
        "github   not answering for {} ({} failed call(s), last {} ago) — scout and \
         build dispatch is held; queued work stays queued and nothing is charged an \
         attempt\n         {}\n",
        humanize(now - hold.since),
        hold.failures,
        humanize(now - hold.last_seen),
        hold.error
    )
}

/// Why the pipeline is idle, when the reason is a half-applied upgrade.
///
/// Silent with nothing pending, for the same reason as the GitHub line. Each
/// reason already names its own discharge (`make restart` / `make images`),
/// so this renders them verbatim rather than summarizing them into a verdict
/// the reader then has to translate back into a command.
pub fn render_update_pending(status: &ServerStatus) -> String {
    let Some(update) = &status.update else {
        return String::new();
    };
    let effect = match update.enforced {
        true => "new scouts and builds wait until it is applied",
        false => "reported only — TASKS_UPDATE_HOLD=off",
    };
    let mut out = format!("update   pending ({effect})\n");
    for reason in &update.reasons {
        out.push_str(&format!("         {reason}\n"));
    }
    out
}

/// Why the pipeline is idle, when the reason is that vm-pool has no room.
///
/// Silent with no hold, for the same reason as the two lines above it. It
/// prints `0 of N` rather than "full", because `0 of 0` is a `VM_POOL_MAX_VMS`
/// that can never dispatch anything and `0 of 6` is work — or a leak — holding
/// every slot, and those want different actions from the reader.
pub fn render_pool_hold(status: &ServerStatus, now: DateTime<Utc>) -> String {
    let Some(hold) = &status.pool else {
        return String::new();
    };
    format!(
        "vm-pool  0 of {} slots free for {} ({} observation(s), last {} ago) — scout \
         and build dispatch waits for one; queued work stays queued and nothing is \
         charged an attempt\n",
        hold.total,
        humanize(now - hold.since),
        hold.observations,
        humanize(now - hold.last_seen),
    )
}

/// How big the orchestrator's verification build directory is.
///
/// **Not silent when things are fine**, which is the one way this differs from
/// the three hold lines above it. A hold is an exception and a standing "all
/// clear" is a line a reader learns to skip; this is a quantity that grows
/// silently, and a row that only appeared once it was over its ceiling would
/// reproduce #1010 exactly — 51 GB found by a human hunting for disk on a
/// filesystem with 74 GiB free.
///
/// Silent only when there is nothing to say: no orchestrator checkout to build
/// in, or no walk yet this boot.
///
/// A reclaim is reported for the rest of the boot, and the wholesale tier names
/// its cost, because "the next verification is cold" is what sends the next
/// batch to a human and nothing else would say why.
pub fn render_verify_dir(status: &ServerStatus, now: DateTime<Utc>) -> String {
    let Some(usage) = &status.verify_dir else {
        return String::new();
    };
    let bound = match usage.budget_bytes {
        Some(budget) if usage.over_budget => format!(
            "over its {} ceiling",
            crate::verify_dir::humanize_bytes(budget)
        ),
        Some(budget) => format!("of {}", crate::verify_dir::humanize_bytes(budget)),
        None => "unbounded — ORCHESTRATOR_TARGET_BUDGET_GB=0, report only".to_string(),
    };
    let mut out = format!(
        "verify   {} {} in {} files, measured {} ago
         {}
",
        crate::verify_dir::humanize_bytes(usage.bytes),
        bound,
        usage.files,
        humanize(now - usage.measured_at),
        usage.path,
    );
    if let Some(reclaim) = &usage.last_reclaim {
        let what = match reclaim.tier {
            tasks_api::http::VerifyDirTier::Incremental => {
                "the incremental caches went (no warmth lost)"
            }
            tasks_api::http::VerifyDirTier::Wholesale => {
                "the whole directory went — the next verification is COLD"
            }
        };
        out.push_str(&format!(
            "         reclaimed {} ago: {} -> {}, {}
",
            humanize(now - reclaim.at),
            crate::verify_dir::humanize_bytes(reclaim.before_bytes),
            crate::verify_dir::humanize_bytes(reclaim.after_bytes),
            what,
        ));
    }
    out
}

/// What the VM images are running, and whether that is a problem.
///
/// **No observation is not a clean bill of health**, so an empty list says
/// "none observed yet" rather than "current": nothing polls an image, so the
/// only way to learn what is in one is to run something inside it.
///
/// Every stale line names `make images`. A verdict word alone tells a reader
/// they have a problem and not what to type — and the rebuild is a host-side
/// command by design, so there is nothing here to click.
pub fn render_images(status: &ServerStatus) -> String {
    if status.images.is_empty() {
        return "images   none observed yet (an image is only read from a run inside it)\n"
            .to_string();
    }
    let mut out = String::from("images\n");
    for image in &status.images {
        let identity = match (&image.version, &image.commit) {
            (Some(version), Some(commit)) => format!("{version} ({commit})"),
            (Some(version), None) => version.clone(),
            _ => "PREDATES STAMPING".to_string(),
        };
        let advice = match image.freshness.needs_rebuild() {
            true => "  — run `make images`",
            false => "",
        };
        out.push_str(&format!(
            "  {}  {}  {}{}\n",
            image.image,
            identity,
            image.freshness.as_str(),
            advice
        ));
    }
    out
}

fn render_in_flight(in_flight: &InFlight, now: DateTime<Utc>) -> String {
    if in_flight.is_empty() {
        return "in flight  nothing\n".to_string();
    }
    let mut out = String::from("in flight\n");
    for item in &in_flight.scouts {
        out.push_str(&render_item("scout", item, now));
    }
    for item in &in_flight.builds {
        out.push_str(&render_item("build", item, now));
    }
    if let Some(item) = &in_flight.orchestrator {
        out.push_str(&render_item("turn ", item, now));
    }
    out
}

fn render_item(kind: &str, item: &tasks_api::http::InFlightItem, now: DateTime<Utc>) -> String {
    match &item.detail {
        Some(detail) => format!(
            "  {kind}  {}  ({detail})  {}\n",
            item.id,
            humanize(now - item.since)
        ),
        None => format!("  {kind}  {}  {}\n", item.id, humanize(now - item.since)),
    }
}

/// What that boot did to the schema — the second question a swap has to
/// answer, and one only the new process can.
pub fn render_migrations(status: &ServerStatus) -> String {
    match status.migrations_applied.as_slice() {
        [] => "migrations: already current".to_string(),
        applied => format!(
            "migrations: applied {} ({})",
            applied.len(),
            applied
                .iter()
                .map(|m| m.file_stem())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// One line naming what is in flight, for a gate message or a drain tick.
fn describe(in_flight: &InFlight) -> String {
    let mut parts = Vec::new();
    if !in_flight.scouts.is_empty() {
        parts.push(plural(in_flight.scouts.len(), "scout"));
    }
    if !in_flight.builds.is_empty() {
        parts.push(plural(in_flight.builds.len(), "build"));
    }
    if in_flight.orchestrator.is_some() {
        parts.push("an owed orchestrator turn".to_string());
    }
    match parts.is_empty() {
        true => "nothing".to_string(),
        false => parts.join(" + "),
    }
}

fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// `45s`, `12m30s`, `1h2m`.
pub fn humanize(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

/// The last few lines of `serve.log`, for a boot that died with something to
/// say. Missing or unreadable reads as empty — this is diagnostics, and a
/// failure to read the log must not replace the failure being reported.
pub(crate) fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_api::http::{AppliedMigration, InFlightItem};

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn pidfile_at(port: u16) -> PidFile {
        PidFile {
            pid: 4242,
            port,
            started_at: ts("2026-08-15T12:00:00Z"),
            exe: PathBuf::from("/usr/local/bin/tasks"),
        }
    }

    fn status_with(in_flight: InFlight) -> ServerStatus {
        ServerStatus {
            pid: 4242,
            started_at: ts("2026-08-15T12:00:00Z"),
            migrations_applied: Vec::new(),
            mode: Mode::Play,
            in_flight,
            images: Vec::new(),
            github: None,
            update: None,
            pool: None,
            verify_dir: None,
        }
    }

    #[test]
    fn ages_read_the_way_a_human_says_them() {
        assert_eq!(humanize(chrono::Duration::seconds(45)), "45s");
        assert_eq!(humanize(chrono::Duration::seconds(750)), "12m30s");
        assert_eq!(humanize(chrono::Duration::seconds(3720)), "1h2m");
        // Clock skew must not render as a negative age.
        assert_eq!(humanize(chrono::Duration::seconds(-5)), "0s");
    }

    #[test]
    fn nothing_running_says_so() {
        let out = render_status(None, None, ts("2026-08-15T12:00:00Z"));
        assert!(out.contains("not serving"), "{out}");
    }

    #[test]
    fn a_live_pid_that_does_not_answer_is_its_own_report() {
        let out = render_status(Some(&pidfile_at(4800)), None, ts("2026-08-15T12:00:00Z"));
        assert!(out.contains("pid 4242 is alive"), "{out}");
        assert!(out.contains("not answering /status"), "{out}");
    }

    #[test]
    fn an_idle_server_renders_uptime_and_no_work() {
        let out = render_status(
            Some(&pidfile_at(4800)),
            Some(&status_with(InFlight::default())),
            ts("2026-08-15T13:02:00Z"),
        );
        assert!(
            out.contains("serving  pid 4242  port 4800  up 1h2m"),
            "{out}"
        );
        assert!(out.contains("mode     play"), "{out}");
        assert!(out.contains("in flight  nothing"), "{out}");
    }

    /// A standing "GitHub ok" line is one a reader learns to skip, so there is
    /// no line at all until there is something to say.
    #[test]
    fn a_healthy_github_says_nothing() {
        let status = status_with(InFlight::default());
        assert_eq!(render_github_hold(&status, ts("2026-08-15T13:00:00Z")), "");
        let whole = render_status(
            Some(&pidfile_at(4800)),
            Some(&status),
            ts("2026-08-15T13:00:00Z"),
        );
        assert!(!whole.contains("github"), "{whole}");
    }

    /// The one time it appears it has to answer the reader's real question —
    /// why is nothing being dispatched, and is work being lost? — and it has to
    /// print both ages, since the gap between them is what says whether anybody
    /// is still refreshing the hold.
    #[test]
    fn a_hold_names_what_is_stopped_and_what_it_costs() {
        use tasks_api::http::GitHubHold;

        let mut status = status_with(InFlight::default());
        status.github = Some(GitHubHold {
            since: ts("2026-08-15T12:48:00Z"),
            last_seen: ts("2026-08-15T12:59:30Z"),
            failures: 12,
            error: "rest: list issues: 503 Service Unavailable: Service Unavailable".into(),
        });

        let line = render_github_hold(&status, ts("2026-08-15T13:00:00Z"));
        assert!(line.contains("12m00s"), "the age of the outage: {line}");
        assert!(line.contains("30s"), "the age of the last look: {line}");
        assert!(line.contains("12 failed call"), "{line}");
        assert!(line.contains("dispatch is held"), "{line}");
        assert!(
            line.contains("nothing is charged an attempt"),
            "the reader's next question is whether work is being lost: {line}"
        );
        assert!(line.contains("503"), "{line}");
        // And it is part of the report, not a function nobody calls.
        assert!(
            render_status(
                Some(&pidfile_at(4800)),
                Some(&status),
                ts("2026-08-15T13:00:00Z")
            )
            .contains("dispatch is held")
        );
    }

    /// The third hold reports like the other two: silent until it binds, and
    /// when it binds it answers "why is nothing dispatching" and "is work
    /// being lost". `0 of N` and not "full" — a pool of zero can never
    /// dispatch and wants a different fix from a pool that is merely busy.
    #[test]
    fn a_full_pool_says_how_full_and_what_it_costs() {
        use tasks_api::http::PoolHold;

        let mut status = status_with(InFlight::default());
        assert_eq!(render_pool_hold(&status, ts("2026-08-15T13:00:00Z")), "");

        status.pool = Some(PoolHold {
            since: ts("2026-08-15T12:48:00Z"),
            last_seen: ts("2026-08-15T12:59:30Z"),
            observations: 138,
            total: 6,
        });
        let line = render_pool_hold(&status, ts("2026-08-15T13:00:00Z"));
        assert!(line.contains("0 of 6"), "{line}");
        assert!(line.contains("12m00s"), "the age of the hold: {line}");
        assert!(line.contains("30s"), "the age of the last look: {line}");
        assert!(
            line.contains("nothing is charged an attempt"),
            "the reader's next question is whether work is being lost: {line}"
        );
        assert!(
            render_status(
                Some(&pidfile_at(4800)),
                Some(&status),
                ts("2026-08-15T13:00:00Z")
            )
            .contains("0 of 6"),
            "part of the report, not a function nobody calls"
        );
    }

    /// The one report here that is **not** an exception: it prints whenever
    /// there is a reading. A row that appeared only once it was over its
    /// ceiling would reproduce #1010 exactly — 51 GB found by a human hunting
    /// for disk on a filesystem with 74 GiB free.
    #[test]
    fn the_verification_build_directory_is_reported_whether_or_not_it_is_a_problem() {
        use tasks_api::http::{VerifyDirReclaim, VerifyDirTier, VerifyDirUsage};

        let mut status = status_with(InFlight::default());
        assert_eq!(
            render_verify_dir(&status, ts("2026-08-15T13:00:00Z")),
            "",
            "nothing measured is silent — it is not a zero"
        );

        let usage = VerifyDirUsage {
            path: "/state/verify-target".into(),
            bytes: 12_300_000_000,
            files: 213_628,
            measured_at: ts("2026-08-15T12:55:00Z"),
            budget_bytes: Some(20_000_000_000),
            over_budget: false,
            last_reclaim: None,
        };
        status.verify_dir = Some(usage.clone());
        let line = render_verify_dir(&status, ts("2026-08-15T13:00:00Z"));
        assert!(line.contains("12.3 GB of 20.0 GB"), "{line}");
        assert!(line.contains("213628 files"), "{line}");
        assert!(line.contains("/state/verify-target"), "{line}");
        assert!(line.contains("5m00s"), "how old the reading is: {line}");
        assert!(
            render_status(
                Some(&pidfile_at(4800)),
                Some(&status),
                ts("2026-08-15T13:00:00Z")
            )
            .contains("12.3 GB"),
            "part of the report, not a function nobody calls"
        );

        // Over the ceiling, and a wholesale reclaim that has to name its cost:
        // a cold verification is what routes the next batch to a human.
        status.verify_dir = Some(VerifyDirUsage {
            bytes: 51_000_000_000,
            over_budget: true,
            last_reclaim: Some(VerifyDirReclaim {
                at: ts("2026-08-15T12:57:00Z"),
                tier: VerifyDirTier::Wholesale,
                before_bytes: 51_000_000_000,
                after_bytes: 0,
            }),
            ..usage.clone()
        });
        let line = render_verify_dir(&status, ts("2026-08-15T13:00:00Z"));
        assert!(line.contains("over its 20.0 GB ceiling"), "{line}");
        assert!(line.contains("51.0 GB -> 0 B"), "{line}");
        assert!(line.contains("COLD"), "{line}");

        // And with the reclaim switched off, the report stays — that half is
        // deliberately not switchable.
        status.verify_dir = Some(VerifyDirUsage {
            budget_bytes: None,
            ..usage
        });
        let line = render_verify_dir(&status, ts("2026-08-15T13:00:00Z"));
        assert!(line.contains("ORCHESTRATOR_TARGET_BUDGET_GB=0"), "{line}");
        assert!(line.contains("12.3 GB"), "{line}");
    }

    /// Two readings that must not be confused. Nothing polls an image, so an
    /// empty list means no run has started in one — reporting that as
    /// "current" would be an answer this server does not have. And every stale
    /// line has to name `make images`: the rebuild is a host-side command, so
    /// a verdict word alone tells a reader they have a problem without telling
    /// them what to type.
    #[test]
    fn no_observation_is_not_a_clean_bill_of_health() {
        use tasks_api::version::{ImageFreshness, ImageIdentity, ImageRole};

        let mut status = status_with(InFlight::default());
        let empty = render_images(&status);
        assert!(empty.contains("none observed yet"), "{empty}");
        assert!(!empty.contains("current"), "{empty}");

        status.images = vec![
            ImageIdentity {
                image: "agent:v1".into(),
                role: ImageRole::Scout,
                version: None,
                commit: None,
                observed_at: Utc::now(),
                run_id: Some("sess_1".into()),
                freshness: ImageFreshness::Unstamped,
            },
            ImageIdentity {
                image: "builder:v1".into(),
                role: ImageRole::Builder,
                version: Some("0.1.163".into()),
                commit: Some("abc1234".into()),
                observed_at: Utc::now(),
                run_id: Some("build_1".into()),
                freshness: ImageFreshness::Current,
            },
        ];
        let rendered = render_images(&status);
        assert!(rendered.contains("PREDATES STAMPING"), "{rendered}");
        assert!(rendered.contains("unstamped"), "{rendered}");
        assert!(rendered.contains("0.1.163 (abc1234)"), "{rendered}");

        // Exactly one line asks for a rebuild — the stale one.
        assert_eq!(
            rendered.matches("make images").count(),
            1,
            "only the stale image asks: {rendered}"
        );
    }

    #[test]
    fn in_flight_work_is_listed_with_its_age() {
        let in_flight = InFlight {
            scouts: vec![InFlightItem {
                id: "sess_1".into(),
                detail: Some("task task_9".into()),
                since: ts("2026-08-15T12:50:00Z"),
            }],
            builds: vec![InFlightItem {
                id: "build_1".into(),
                detail: Some("tasks/build-1".into()),
                since: ts("2026-08-15T12:59:30Z"),
            }],
            orchestrator: Some(InFlightItem {
                id: "17".into(),
                detail: None,
                since: ts("2026-08-15T12:59:15Z"),
            }),
        };
        let out = render_status(
            Some(&pidfile_at(4800)),
            Some(&status_with(in_flight)),
            ts("2026-08-15T13:00:00Z"),
        );
        assert!(out.contains("sess_1  (task task_9)  10m00s"), "{out}");
        assert!(out.contains("build_1  (tasks/build-1)  30s"), "{out}");
        assert!(out.contains("17  45s"), "{out}");
    }

    #[test]
    fn only_scouts_and_builds_are_destructible() {
        let owed = InFlight {
            orchestrator: Some(InFlightItem {
                id: "3".into(),
                detail: None,
                since: ts("2026-08-15T12:00:00Z"),
            }),
            ..InFlight::default()
        };
        assert!(!owed.is_destructible(), "an owed turn must never gate");
        assert!(!owed.is_empty());

        let scouting = InFlight {
            scouts: vec![InFlightItem {
                id: "sess_1".into(),
                detail: None,
                since: ts("2026-08-15T12:00:00Z"),
            }],
            ..InFlight::default()
        };
        assert!(scouting.is_destructible());
    }

    #[test]
    fn the_gate_message_names_what_is_running() {
        let in_flight = InFlight {
            scouts: vec![
                InFlightItem {
                    id: "a".into(),
                    detail: None,
                    since: ts("2026-08-15T12:00:00Z"),
                },
                InFlightItem {
                    id: "b".into(),
                    detail: None,
                    since: ts("2026-08-15T12:00:00Z"),
                },
            ],
            builds: vec![InFlightItem {
                id: "c".into(),
                detail: None,
                since: ts("2026-08-15T12:00:00Z"),
            }],
            orchestrator: None,
        };
        assert_eq!(describe(&in_flight), "2 scouts + 1 build");
        assert_eq!(describe(&InFlight::default()), "nothing");
    }

    #[test]
    fn migrations_are_reported_by_filename() {
        let mut status = status_with(InFlight::default());
        assert_eq!(render_migrations(&status), "migrations: already current");

        status.migrations_applied = vec![
            AppliedMigration {
                version: 2,
                // sqlx stores the description with underscores as spaces.
                description: "manual rank".into(),
            },
            AppliedMigration {
                version: 19,
                description: "charter comment and land".into(),
            },
        ];
        assert_eq!(
            render_migrations(&status),
            "migrations: applied 2 (0002_manual_rank, 0019_charter_comment_and_land)"
        );
    }

    /// The restart/stop asymmetry, in the one place it is written down: a
    /// restart's pause is undone by the swap, a stop's outlives the command —
    /// and a timeout did neither thing, whichever caller asked.
    #[test]
    fn a_drain_says_what_it_owes_the_mode() {
        assert!(ModeAfterDrain::Restored.pause_note().contains("new server"));
        assert!(
            ModeAfterDrain::LeftPaused
                .pause_note()
                .contains("stays paused"),
            "{}",
            ModeAfterDrain::LeftPaused.pause_note()
        );
        assert_eq!(
            ModeAfterDrain::Restored.nothing_happened(),
            "nothing was restarted"
        );
        assert_eq!(
            ModeAfterDrain::LeftPaused.nothing_happened(),
            "nothing was stopped"
        );
    }

    /// The third caller answers the question the other two answer differently:
    /// its pause is the deliverable, so it says "until `tasks resume`" — and
    /// its timeout sentence is imperative, because the next thing that
    /// operator does is delete containers.
    #[test]
    fn a_maintenance_drain_holds_the_pause_and_says_so_on_the_feed() {
        let held = ModeAfterDrain::HeldForMaintenance;
        assert!(
            held.pause_note().contains("tasks resume"),
            "{}",
            held.pause_note()
        );
        assert!(
            held.nothing_happened().contains("not quiesced")
                && held.nothing_happened().contains("do not restart vm-pool"),
            "{}",
            held.nothing_happened()
        );
        // The feed half: only this variant has anything to say there. A
        // restart's pause is undone by the swap seconds later and a stop's is
        // announced by the stop itself; this one leaves a `pause` nothing can
        // tell from any other.
        assert!(held.feed_note().is_some_and(|n| n.contains("tasks resume")));
        assert!(
            held.restore_note()
                .is_some_and(|n| n.contains("nothing is held"))
        );
        for quiet in [ModeAfterDrain::Restored, ModeAfterDrain::LeftPaused] {
            assert!(quiet.feed_note().is_none());
            assert!(quiet.restore_note().is_none());
        }
    }

    /// `--check` holds nothing, so it must not borrow the held drain's closing
    /// words: the first cut returned `Quiesced { left_paused: false }` for a
    /// clear check, and told an idle-but-*playing* server "dispatch was
    /// already not playing".
    #[test]
    fn only_a_held_pipeline_gets_the_held_pipelines_words() {
        assert_eq!(render_quiesced(&Drained::Clear), "");
        assert_eq!(render_quiesced(&Drained::NotServing), "");

        let held = render_quiesced(&Drained::Quiesced {
            port: 4811,
            left_paused: true,
        });
        assert!(held.contains("stays paused"), "{held}");
        assert!(held.contains("make images"), "{held}");
        assert!(held.contains("tasks resume"), "{held}");
        assert!(held.contains("localhost:4811/mode"), "{held}");

        let already = render_quiesced(&Drained::Quiesced {
            port: 4811,
            left_paused: false,
        });
        assert!(already.contains("already not playing"), "{already}");
        assert!(already.contains("tasks resume"), "{already}");
    }

    /// The gate `make images` runs on, in its pure half. Nothing in flight is
    /// **not** enough: a playing pipeline tops scouts up on the next tick, and
    /// a scout that starts during a rebuild starts in the old image.
    #[test]
    fn a_check_refuses_a_playing_pipeline_as_well_as_a_busy_one() {
        let mut status = status_with(InFlight::default());

        status.mode = Mode::Play;
        let err = check(&status).expect_err("a playing pipeline is not quiesced");
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("tasks drain"), "{err}");

        status.mode = Mode::Pause;
        check(&status).expect("idle and not playing is the clear case");

        status.mode = Mode::Stop;
        check(&status).expect("stop dispatches nothing either");

        status.mode = Mode::Pause;
        status.in_flight = InFlight {
            scouts: vec![InFlightItem {
                id: "sess_1".into(),
                detail: None,
                since: ts("2026-08-15T12:00:00Z"),
            }],
            ..InFlight::default()
        };
        let err = check(&status).expect_err("work in flight is not quiesced either");
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("1 scout in flight"), "{err}");
    }

    /// `--cancel-scouts` is opt-in, and the drain waits as long as a reload
    /// would by default.
    #[test]
    fn a_drain_waits_and_cancels_nothing_unless_asked() {
        let opts = DrainOptions::default();
        assert!(!opts.check);
        assert!(!opts.cancel_scouts);
        assert_eq!(opts.drain_timeout, DEFAULT_DRAIN_TIMEOUT);
    }

    /// The lasting consequence of a `--when-idle` stop comes with its undo,
    /// on the port the server was actually on.
    #[test]
    fn a_left_paused_pipeline_is_reported_with_the_curl_that_undoes_it() {
        let note = render_left_paused(4811);
        assert!(note.contains("paused"), "{note}");
        assert!(note.contains("localhost:4811/mode"), "{note}");
        assert!(note.contains("\"mode\":\"play\""), "{note}");
    }

    /// A stop defaults to what it always did: immediate, ungated, and with the
    /// same drain budget as `reload` when asked to wait.
    #[test]
    fn a_stop_is_immediate_unless_asked_otherwise() {
        let opts = StopOptions::default();
        assert!(!opts.when_idle);
        assert_eq!(opts.drain_timeout, DEFAULT_DRAIN_TIMEOUT);
    }

    #[test]
    fn exit_codes_are_distinct_per_failure() {
        assert_eq!(ReloadError::Busy(String::new()).exit_code(), 3);
        assert_eq!(ReloadError::DrainTimeout(String::new()).exit_code(), 4);
        assert_eq!(ReloadError::SwapFailed(String::new()).exit_code(), 5);
        assert_eq!(ReloadError::BuildFailed.exit_code(), 1);
    }

    #[test]
    fn the_artifact_is_read_out_of_cargos_json() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"serde"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"tasks"},"executable":"/w/target/debug/tasks"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        assert_eq!(
            artifact_path(stdout),
            Some(PathBuf::from("/w/target/debug/tasks"))
        );
        assert_eq!(artifact_path("not json at all\n"), None);
    }

    #[test]
    fn the_workspace_is_found_from_anywhere_inside_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/tasks")).unwrap();
        std::fs::write(root.path().join("crates/tasks/Cargo.toml"), "").unwrap();
        let deep = root.path().join("target/debug");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            workspace_above(&deep.join("tasks")).map(|p| p.canonicalize().unwrap()),
            Some(root.path().canonicalize().unwrap())
        );
        assert!(workspace_above(Path::new("/")).is_none());
    }

    #[test]
    fn a_log_tail_survives_a_missing_log() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(log_tail(&dir.path().join("serve.log")), "");

        let log = dir.path().join("serve.log");
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        std::fs::write(&log, lines.join("\n")).unwrap();
        let tail = log_tail(&log);
        assert!(tail.starts_with("line 30"), "{tail}");
        assert!(tail.ends_with("line 49"), "{tail}");
    }
}
