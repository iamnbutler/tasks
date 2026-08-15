//! `tasks reload` — build, report, drain, swap, verify.
//!
//! The upgrade loop the server was missing. The order is the whole point:
//!
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
//! This is not a service manager: no supervision, no restart-on-crash, no
//! daemon. The pidfile is a discovery record and the swap is one-shot;
//! `launchd`/`systemd` compose with it fine, pointed at `tasks serve`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tasks_api::http::{InFlight, ModeResponse, ServerStatus, SetMode};
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

/// Build, report, gate, drain, swap, verify. See the module docs for why in
/// that order.
pub async fn reload(opts: ReloadOptions) -> Result<(), ReloadError> {
    // 1. Build first: a failure here must cost nothing, and it can only cost
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

    let Some(existing) = existing else {
        // Nothing is running: a reload is just a start.
        return start(&binary, port, &opts, None).await;
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
        return swap(&binary, &existing, port, &opts, None).await;
    };

    let mut restore_mode = None;
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
        restore_mode = drain(existing.port, status.mode, opts.drain_timeout).await?;
    }

    if status.in_flight.orchestrator.is_some() {
        println!(
            "note: an orchestrator turn is owed; the restart costs it one turn \
             (the next boot takes it again)"
        );
    }

    swap(&binary, &existing, port, &opts, restore_mode).await
}

/// SIGTERM the old server, start the new one, verify, restore the mode.
async fn swap(
    binary: &Path,
    existing: &PidFile,
    port: u16,
    opts: &ReloadOptions,
    restore_mode: Option<Mode>,
) -> Result<(), ReloadError> {
    println!("stopping pid {}…", existing.pid);
    stop_pid(existing.pid).await?;
    println!("stopped");
    match start(binary, port, opts, restore_mode).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(mode) = restore_mode {
                // The pipeline is paused and the server that could unpause it
                // did not come up. Say so, with the undo.
                println!(
                    "the pipeline is still paused; once a server is up: \
                     curl -sS -X POST localhost:{port}/mode -H 'content-type: application/json' \
                     -d '{{\"mode\":\"{}\"}}'",
                    mode.as_str()
                );
            }
            Err(err)
        }
    }
}

/// Launch the new server and prove it is the one answering.
async fn start(
    binary: &Path,
    port: u16,
    opts: &ReloadOptions,
    restore_mode: Option<Mode>,
) -> Result<(), ReloadError> {
    if opts.foreground {
        // exec, so the log in this terminal is the server's own and ctrl-c
        // reaches it directly. Nothing after this line runs — including a
        // mode restore, so say so rather than leaving it paused in silence.
        if let Some(mode) = restore_mode {
            println!(
                "the drain paused dispatch; once this server is up: \
                 curl -sS -X POST localhost:{port}/mode \
                 -H 'content-type: application/json' -d '{{\"mode\":\"{}\"}}'",
                mode.as_str()
            );
        }
        return Err(exec_foreground(binary, port, &opts.data_dir));
    }

    let log_path = opts.data_dir.join("serve.log");
    tokio::fs::create_dir_all(&opts.data_dir).await?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut child = Command::new(binary)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .env("TASKS_DATA_DIR", &opts.data_dir)
        .current_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()?;
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

    // Restoring the mode only after the new server answers is the only
    // correct order: mode lives in the store, so it survives the restart, and
    // unpausing before the swap landed would let the old server dispatch.
    if let Some(mode) = restore_mode {
        match set_mode(port, mode).await {
            Ok(()) => println!("mode restored to {}", mode.as_str()),
            Err(err) => println!("could not restore mode to {}: {err}", mode.as_str()),
        }
    }
    Ok(())
}

/// Replace this process with `tasks serve`. Only returns on failure.
fn exec_foreground(binary: &Path, port: u16, data_dir: &Path) -> ReloadError {
    use std::os::unix::process::CommandExt;
    println!("exec {} serve --port {port}", binary.display());
    let err = std::process::Command::new(binary)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .env("TASKS_DATA_DIR", data_dir)
        .exec();
    ReloadError::Io(err)
}

/// Pause dispatch, wait for destructible work to finish, and report the mode
/// that has to be restored once the new server is up (`None` when the
/// pipeline was not playing to begin with).
async fn drain(port: u16, mode: Mode, timeout: Duration) -> Result<Option<Mode>, ReloadError> {
    let restore = match mode {
        Mode::Play => {
            set_mode(port, Mode::Pause)
                .await
                .map_err(ReloadError::Other)?;
            println!("paused dispatch for the drain (mode will be restored to play)");
            Some(Mode::Play)
        }
        // Already not dispatching; nothing to pause and nothing to restore.
        other => {
            println!("mode is {}; nothing new will be dispatched", other.as_str());
            None
        }
    };

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    loop {
        match fetch_status(port).await {
            Ok(status) if !status.in_flight.is_destructible() => {
                println!("drained");
                return Ok(restore);
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
            // Restore immediately: this restarts nothing, so leaving the
            // pipeline paused would be a side effect of a no-op.
            if let Some(mode) = restore
                && let Err(err) = set_mode(port, mode).await
            {
                println!("could not restore mode to {}: {err}", mode.as_str());
            }
            return Err(ReloadError::DrainTimeout(format!(
                "still busy after {}s; nothing was restarted",
                timeout.as_secs()
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
pub async fn stop(data_dir: &Path) -> Result<Option<PidFile>, ReloadError> {
    let Some(file) = pidfile::read_live(data_dir) else {
        return Ok(None);
    };
    stop_pid(file.pid).await?;
    // Belt and braces: the server clears its own record on the way out, but a
    // SIGKILLed one cannot.
    pidfile::remove_if_ours(data_dir, file.pid);
    Ok(Some(file))
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

async fn wait_gone(pid: u32, budget: Duration) -> bool {
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

fn workspace_above(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("crates/tasks/Cargo.toml").is_file())
        .map(PathBuf::from)
}

// --- probes ---

async fn fetch_status(port: u16) -> Result<ServerStatus, String> {
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

async fn set_mode(port: u16, mode: Mode) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/mode"))
        .json(&SetMode {
            mode: mode.as_str().to_string(),
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
            out.push_str(&render_in_flight(&status.in_flight, now));
        }
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
fn log_tail(path: &Path) -> String {
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
