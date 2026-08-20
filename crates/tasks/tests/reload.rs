//! The upgrade loop, end to end: real `tasks` binaries, real SQLite, real
//! signals. Every test drives the CLI as a subprocess rather than calling
//! [`tasks::reload`] in-process, because `--no-build` swaps in
//! `current_exe()` — in-process that would be the test harness, and the thing
//! under test is precisely "which binary ends up serving".
//!
//! Liveness is always asserted with [`tasks::pidfile::pid_alive`], never with
//! `Child::try_wait`: these children are not reaped promptly (the test is
//! blocked inside `reload`), so "has it exited" and "has it been reaped"
//! differ, and the reap races with tokio's SIGCHLD handling.
//!
//! Four environment settings are forced on every child, and each closes a
//! route by which ambient configuration decides a result here.
//! `TASKS_DEFAULT_MODE` decides what a boot comes up in. `ORCHESTRATOR_CMD` is
//! pointed at a stub: the default is `claude`, so on any machine that has it
//! installed the mode flips below started a live agent turn that the shutdown
//! then waited out — minutes of wall clock spent on nothing, in a suite about
//! restarts. `TASKS_ENV_FILES=off`, because `env_remove` is the *opposite*
//! of a scrub: these children are real `tasks` processes, `main` runs
//! `env_file::load()`, and the real environment is the only thing a `.env`
//! entry loses to — so removing a variable is exactly what lets this
//! checkout's (gitignored, so per-machine) `.env` decide it. And
//! `TASKS_VM_POOL_AUTOSPAWN=off`, because unset it is *derived from where the
//! binary lives* — and this binary does not always live in the checkout. Under
//! `make test` it does and the derivation answers `off`, which is why the
//! omission was invisible; under a relocated `CARGO_TARGET_DIR` (the
//! orchestrator's verification runs, `ORCHESTRATOR_TARGET_DIR`) it reads as
//! *installed*, and every `serve` boot here spawned a detached vm-pool onto
//! the tempdir socket — own process group, no pidfile for [`DataDir`]'s Drop
//! to find, unique socket so the occupied-socket refusal never fires. One
//! leaked daemon per boot, ~350 found live on 2026-08-19 (#1038). The leak is
//! the visible half: the pool also *binds* the socket `serve_command`
//! deliberately points at nothing, turning "dispatch off" into dispatch
//! against a real pool in exactly one environment.
//! [`a_test_server_spawns_no_vm_pool`] pins the property where the filesystem
//! can see it.
//!
//! # The budget
//!
//! nextest kills a test at 60s (`.config/nextest.toml`: `slow-timeout` 5s ×
//! `terminate-after` 12), and a killed test prints none of its own assertions
//! — it surfaces as an opaque `TIMEOUT`. So [`DRAIN_TIMEOUT`] sits well under
//! that: at 60 it sat *exactly* on the threshold, and a drain that genuinely
//! timed out could never say so.
//!
//! One thing can blow the budget while every assertion here is correct, and it
//! is the first thing to check if this suite ever times out again: `reload`'s
//! own `STOP_GRACE` is **75s**, permanently larger than the whole harness
//! budget. A server that misses its graceful shutdown path is SIGKILLed at 75s,
//! which this suite can only ever observe as a harness timeout with no output.
//! That is what #883 was — a background loop that never saw the shutdown flag,
//! awaited unbounded by a drain that named nothing. The server now bounds and
//! names those loops (`run::drain_background`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tasks::models::{
    GhState, Mode, Project, ProjectId, ProjectStatus, RunKind, Session, SessionId, SessionStatus,
    Task, TaskId, TaskState,
};
use tasks::pidfile;
use tasks::store::Store;
use tasks_api::http::ServerStatus;
use tempfile::TempDir;
use tokio::process::Command;

fn tasks_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tasks"))
}

/// `--drain-timeout` for the tests that wait one out. Comfortably under
/// nextest's 60s kill, so a drain that really does time out can print its own
/// failure instead of dying as a `TIMEOUT` with no output. See the module docs.
const DRAIN_TIMEOUT: &str = "20";

/// A data dir that takes its server down with it, however the test ended.
/// The servers `reload` starts are in their own process group and are nobody's
/// children, so nothing else would reap them.
struct DataDir(TempDir);

impl DataDir {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        if let Some(file) = pidfile::read(self.0.path()) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &file.pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// The same trick from a non-async caller — `serve_command` is sync, and
/// making it async would ripple through every test in this file for one
/// environment variable.
fn blocking_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// An orchestrator that costs nothing: a shell script that reads its prompt
/// and answers. Without it, `ORCHESTRATOR_CMD` defaults to `claude` and every
/// nudge-worthy event in this file spawns a real agent turn.
fn stub_orchestrator(data_dir: &Path) -> PathBuf {
    let stub = data_dir.join("stub-orchestrator.sh");
    if !stub.exists() {
        std::fs::write(&stub, "#!/bin/sh\ncat > /dev/null\necho ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    stub
}

/// `tasks serve`, wired to nothing external: no GitHub token (polling off) and
/// a vm-pool socket that does not exist (dispatch off, API up).
fn serve_command(data_dir: &Path, port: u16) -> Command {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("test-serve.log"))
        .unwrap();
    let mut cmd = Command::new(tasks_bin());
    cmd.args(["serve", "--port", &port.to_string()])
        .env("TASKS_DATA_DIR", data_dir)
        .env("VM_POOL_SOCKET", data_dir.join("vm-pool.sock"))
        .env("ORCHESTRATOR_CMD", stub_orchestrator(data_dir))
        // The broker binds right after the API does and a clash there is a
        // boot failure, so it needs the same per-test port the API gets: the
        // default 4801 is one fixed port shared by every server this file
        // starts, and nextest runs these tests concurrently. Loopback rather
        // than the default `0.0.0.0` for a second reason — binding every
        // interface raises the macOS firewall prompt, once per test binary.
        .env("TASKS_BROKER_PORT", blocking_free_port().to_string())
        .env("TASKS_BROKER_BIND", "127.0.0.1")
        // Without this the `env_remove` below hands the decision to whichever
        // `.env` this checkout happens to have. See the module docs.
        .env(tasks::env_file::DISABLE_VAR, "off")
        // Unset, this derives from where the binary lives, and under a
        // relocated target dir a test binary reads as installed — one leaked
        // detached pool per boot, bound to the socket this command points at
        // nothing (#1038). See the module docs.
        .env("TASKS_VM_POOL_AUTOSPAWN", "off")
        .env_remove("GITHUB_TOKEN")
        .env_remove("TASKS_DEFAULT_MODE")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    cmd
}

/// Start a server and wait until it answers with its own pid.
async fn start_server(data_dir: &Path, port: u16) -> (tokio::process::Child, ServerStatus) {
    let child = serve_command(data_dir, port).spawn().unwrap();
    let status = wait_serving(port).await;
    (child, status)
}

async fn wait_serving(port: u16) -> ServerStatus {
    for _ in 0..300 {
        if let Some(status) = fetch_status(port).await {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no server answered /status on port {port}");
}

async fn fetch_status(port: u16) -> Option<ServerStatus> {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/status"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

async fn set_mode(port: u16, mode: Mode) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mode"))
        .json(&serde_json::json!({ "mode": mode.as_str() }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

async fn mode(port: u16) -> Mode {
    fetch_status(port).await.expect("a serving server").mode
}

/// Run the CLI against `data_dir` and return (exit code, stdout, stderr).
async fn cli(data_dir: &Path, args: &[&str]) -> (i32, String, String) {
    cli_with(data_dir, args, None).await
}

/// The same, with an explicit `TASKS_DEFAULT_MODE` — which the server `reload`
/// spawns inherits, exactly as it would from an operator's shell or a `.env`.
///
/// The successor `reload` spawns inherits this environment wholesale, so the
/// `.env` switch set here covers it too.
async fn cli_with(
    data_dir: &Path,
    args: &[&str],
    default_mode: Option<&str>,
) -> (i32, String, String) {
    let mut cmd = Command::new(tasks_bin());
    cmd.args(args)
        .env("TASKS_DATA_DIR", data_dir)
        .env("VM_POOL_SOCKET", data_dir.join("vm-pool.sock"))
        .env("ORCHESTRATOR_CMD", stub_orchestrator(data_dir))
        // The successor `reload` spawns inherits this environment, so it
        // needs its own broker port for the reason `serve_command` does.
        .env("TASKS_BROKER_PORT", blocking_free_port().to_string())
        .env("TASKS_BROKER_BIND", "127.0.0.1")
        .env(tasks::env_file::DISABLE_VAR, "off")
        // The successor `reload` spawns inherits this too — the same route
        // the `.env` switch above rides. Without it, the server a reload
        // test brings up autospawns a pool the test's server did not (#1038).
        .env("TASKS_VM_POOL_AUTOSPAWN", "off")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::null());
    match default_mode {
        Some(mode) => cmd.env("TASKS_DEFAULT_MODE", mode),
        None => cmd.env_remove("TASKS_DEFAULT_MODE"),
    };
    let output = cmd.output().await.unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Put a `running` scout session in the store behind the server's back — the
/// same row a live scout would have, and the one the drain gate reads.
/// Inserted after the server is up, so startup reconciliation cannot clear it.
async fn insert_running_session(data_dir: &Path) -> SessionId {
    let store = Store::open(data_dir.join("tasks.db")).await.unwrap();
    let project = Project {
        id: ProjectId::new(),
        repo_owner: "iamnbutler".into(),
        repo_name: "tasks".into(),
        added_at: chrono::Utc::now(),
        status: ProjectStatus::Active,
    };
    store.insert_project(&project).await.unwrap();
    let now = chrono::Utc::now();
    let task = Task {
        id: TaskId::new(),
        project_id: project.id.clone(),
        gh_issue_number: 1,
        title: "in flight".into(),
        body: String::new(),
        labels: vec![],
        gh_state: GhState::Open,
        state: TaskState::Scouting,
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
        vm_id: Some("vm-test".into()),
        branch: "scout/1".into(),
        status: SessionStatus::Running,
        started_at: now - chrono::Duration::minutes(20),
        completed_at: None,
        exit_reason: None,
        usage: None,
        directions: None,
    };
    store.insert_session(&session).await.unwrap();
    session.id
}

async fn finish_session(data_dir: &Path, id: &SessionId) {
    let store = Store::open(data_dir.join("tasks.db")).await.unwrap();
    store
        .update_session_completion(id, SessionStatus::ScoutSucceeded, chrono::Utc::now(), None)
        .await
        .unwrap();
}

/// The `mode`-sourced notes on the feed, which is where a drain and a resume
/// record their edges — the mode itself is the standing answer, and there is
/// deliberately nothing between them.
async fn drain_notes(data_dir: &Path) -> Vec<String> {
    let store = Store::open(data_dir.join("tasks.db")).await.unwrap();
    store
        .all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            tasks::events::EventPayload::Note { source, message } if source == "mode" => {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

async fn wait_until_gone(pid: u32) {
    for _ in 0..300 {
        if !pidfile::pid_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid {pid} never exited");
}

// --- tests ---

/// The hole this suite's header used to claim was closed: `env_remove` does not
/// scrub a variable, it *promotes the `.env` that defines it*. These children
/// are real `tasks` processes and `main` runs `env_file::load()`, so a
/// maintainer with `TASKS_DEFAULT_MODE=play` in a (gitignored) `.env` failed
/// this file on their machine and nowhere else.
///
/// Both halves are load-bearing. The control proves the file really can decide
/// the boot mode; without it the second assertion passes whether or not the
/// switch does anything at all.
#[tokio::test]
async fn a_dot_env_decides_the_boot_mode_unless_the_switch_is_off() {
    let dir = DataDir::new();
    // `<data dir>/.env` is the first place `env_file` looks, and the one this
    // test can control — the other two are the cwd's and the executable's
    // checkout.
    std::fs::write(dir.path().join(".env"), "TASKS_DEFAULT_MODE=play\n").unwrap();

    // Control: with the switch removed, `serve_command`'s `env_remove` of
    // TASKS_DEFAULT_MODE is exactly what lets the file decide.
    let port = free_port().await;
    let mut cmd = serve_command(dir.path(), port);
    cmd.env_remove(tasks::env_file::DISABLE_VAR);
    let mut child = cmd.spawn().unwrap();
    let status = wait_serving(port).await;
    assert_eq!(
        status.mode,
        Mode::Play,
        "the .env really can decide the boot mode — without this the assertion \
         below is vacuous"
    );
    cli(dir.path(), &["stop"]).await;
    wait_until_gone(status.pid).await;
    let _ = child.start_kill();

    // And with the settings this suite uses, the same file decides nothing.
    let port = free_port().await;
    let (mut child, status) = start_server(dir.path(), port).await;
    assert_eq!(
        status.mode,
        Mode::Pause,
        "TASKS_ENV_FILES=off must skip the file the control just proved wins"
    );
    cli(dir.path(), &["stop"]).await;
    wait_until_gone(status.pid).await;
    let _ = child.start_kill();
}

/// The happy path: one command, and a different process is serving the same
/// port from the same data dir — with the schema question answered by the new
/// process rather than assumed.
/// A test server must bring no vm-pool up with it (#1038). The autospawn
/// default derives from where the binary lives, so under a relocated
/// `CARGO_TARGET_DIR` — the orchestrator's verification runs — this binary
/// reads as installed and an unset `TASKS_VM_POOL_AUTOSPAWN` spawned one
/// detached pool per `serve` boot: no pidfile for [`DataDir`] to kill, a
/// unique socket so the occupied-socket refusal never fired, ~350 found live.
/// The observable is the filesystem, not the env line: an autospawn creates
/// `vm-pool.log` before it spawns and the pool then binds the socket
/// (`tests/autospawn.rs` proves that half with the switch `on`). Under a
/// checkout run the derivation answers `off` anyway, so this is deliberately
/// a canary for the one environment where the omission bites — which is also
/// the environment that runs it most.
#[tokio::test]
async fn a_test_server_spawns_no_vm_pool() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut child, status) = start_server(dir.path(), port).await;

    // The failed connect that would trigger an autospawn happens at boot,
    // ahead of /status answering; the grace covers scheduling slop, not a
    // window we are racing.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let log = dir.path().join("vm-pool.log");
    let socket = dir.path().join("vm-pool.sock");
    let spawned_log = log.exists();
    let bound_socket = socket.exists();

    cli(dir.path(), &["stop"]).await;
    wait_until_gone(status.pid).await;
    let _ = child.start_kill();

    assert!(
        !spawned_log,
        "the server wrote {} — it autospawned a vm-pool, which nothing in \
         this suite will ever reap",
        log.display()
    );
    assert!(
        !bound_socket,
        "something bound {} — a pool came up on the socket this suite \
         deliberately points at nothing",
        socket.display()
    );
}

#[tokio::test]
async fn an_idle_swap_replaces_the_process() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;

    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("in flight  nothing"), "{stdout}");
    assert!(stdout.contains("up: pid"), "{stdout}");
    // Second boot over the same database: nothing to migrate, and it says so
    // rather than staying silent about the schema.
    assert!(stdout.contains("migrations: already current"), "{stdout}");

    wait_until_gone(before.pid).await;
    let after = wait_serving(port).await;
    assert_ne!(after.pid, before.pid, "a new process is serving");

    let file = pidfile::read(dir.path()).expect("a pidfile");
    assert_eq!(file.pid, after.pid);
    assert_eq!(file.port, port);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// A scout in flight is *reported* and never refused: `resume_in_flight`
/// re-attaches to every live VM, so the worst a swap costs is one write-off
/// that charges no attempt. `--force` is not named, because there is no longer
/// a refusal for it to be the way past.
#[tokio::test]
async fn a_scout_in_flight_does_not_refuse_the_swap() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    // Reported, with its age, on the way past.
    assert!(stdout.contains("1 scout in flight"), "{stdout}");
    assert!(stdout.contains("--when-idle"), "{stdout}");
    assert!(
        !stdout.contains("--force") && !stderr.contains("--force"),
        "--force is no longer the way past work in flight\n{stdout}{stderr}"
    );

    wait_until_gone(before.pid).await;
    let after = wait_serving(port).await;
    assert_ne!(after.pid, before.pid);
    // The new boot reconciled the scout it interrupted.
    assert!(after.in_flight.scouts.is_empty(), "{:?}", after.in_flight);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The pause is proved by the *held command itself* reading `/mode` back —
/// not by the test process sampling it, which would be testing its own
/// scheduler. And the mode comes back although the command exited 7, because
/// the whole reason the child is ours is that its failure must not strand the
/// hold.
#[tokio::test]
async fn a_hold_pauses_for_exactly_as_long_as_its_command_runs() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let seen = dir.path().join("mode-during");
    let script = dir.path().join("held.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncurl -sS localhost:{port}/mode > {}\nexit 7\n",
            seen.display()
        ),
    )
    .unwrap();
    let (code, stdout, stderr) = cli(
        dir.path(),
        &[
            "hold",
            "--label",
            "a test",
            "--",
            "sh",
            script.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 7, "the child's status, verbatim\n{stdout}{stderr}");
    let during = std::fs::read_to_string(&seen).unwrap();
    assert!(
        during.contains("pause"),
        "the command ran with dispatch held: {during}"
    );
    assert!(stdout.contains("a test"), "the label is echoed: {stdout}");
    assert!(
        stdout.contains("dispatch is playing again"),
        "and the mode came back although the child failed: {stdout}"
    );
    assert_eq!(fetch_status(port).await.unwrap().mode, Mode::Play);

    // Both edges are on the feed, so an hour later something says why the
    // mode moved twice.
    let notes = drain_notes(dir.path()).await;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("`tasks hold`") && n.contains("host command")),
        "the pause edge: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("`tasks hold`") && n.contains("back to play")),
        "the restore edge: {notes:?}"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// A scout in flight is not waited for and not cancelled: what a rebuild can
/// spoil is a run dispatched *into* it, and one that started earlier is not
/// that case.
#[tokio::test]
async fn a_hold_neither_waits_for_nor_cancels_a_running_scout() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    let session = insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(dir.path(), &["hold", "--", "true"]).await;
    assert_eq!(code, 0, "{stdout}{stderr}");
    let status = fetch_status(port).await.unwrap();
    assert!(
        status
            .in_flight
            .scouts
            .iter()
            .any(|s| s.id == session.to_string()),
        "the scout is still running: {:?}",
        status.in_flight
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// With nothing serving the command still runs — a gate that only worked on a
/// host that happens to be serving would be no gate at all — and a pipeline
/// that was not playing is left exactly as it is rather than promoted.
#[tokio::test]
async fn a_hold_runs_the_command_whatever_state_the_host_is_in() {
    let dir = DataDir::new();
    let (code, stdout, stderr) = cli(dir.path(), &["hold", "--", "true"]).await;
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("not serving"), "{stdout}");

    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Stop).await;
    let (code, stdout, stderr) = cli(dir.path(), &["hold", "--", "false"]).await;
    assert_eq!(code, 1, "still the child's status\n{stdout}{stderr}");
    assert_eq!(
        fetch_status(port).await.unwrap().mode,
        Mode::Stop,
        "`stop` is tighter than `pause`; restoring it would turn intake back on"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// Flags are read only ahead of `--`; everything after it is the command,
/// verbatim, including its own flags.
#[tokio::test]
async fn a_holds_flags_stop_at_the_double_dash() {
    let dir = DataDir::new();
    let out = dir.path().join("argv");
    let script = dir.path().join("argv.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" > {}\n", out.display()),
    )
    .unwrap();

    let (code, stdout, stderr) = cli(
        dir.path(),
        &[
            "hold",
            "--",
            "sh",
            script.to_str().unwrap(),
            "--label",
            "-j4",
        ],
    )
    .await;
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "--label -j4");

    // And nothing to run is a usage error rather than a silent hold.
    let (code, _, stderr) = cli(dir.path(), &["hold", "--"]).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("nothing to run"), "{stderr}");
}

/// `--when-idle` pauses dispatch (without which the wait never terminates),
/// waits for the work to land, swaps, and hands the *pre-drain* mode to the
/// new server — the pause it installed is the tool, not the intent.
#[tokio::test]
async fn when_idle_waits_for_the_drain_and_restores_the_mode() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    let session = insert_running_session(dir.path()).await;

    let data_dir = dir.path().to_path_buf();
    let reload = tokio::spawn(async move {
        cli(
            &data_dir,
            &[
                "reload",
                "--no-build",
                "--when-idle",
                "--drain-timeout",
                DRAIN_TIMEOUT,
            ],
        )
        .await
    });

    // It pauses before it waits.
    let mut paused = false;
    for _ in 0..100 {
        if mode(port).await == Mode::Pause {
            paused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(paused, "the drain must pause dispatch or it never ends");

    // The scout lands; the swap follows.
    finish_session(dir.path(), &session).await;

    let (code, stdout, stderr) = reload.await.unwrap();
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("paused dispatch"), "{stdout}");
    assert!(stdout.contains("drained"), "{stdout}");
    assert!(stdout.contains("mode carried over: play"), "{stdout}");

    wait_until_gone(before.pid).await;
    let after = wait_serving(port).await;
    assert_ne!(after.pid, before.pid);
    assert_eq!(after.mode, Mode::Play, "the mode came back after the swap");

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// A drain that gives up restarts nothing and leaves the pipeline exactly as
/// it found it — a no-op must not have side effects.
#[tokio::test]
async fn a_drain_that_times_out_restarts_nothing() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(
        dir.path(),
        &[
            "reload",
            "--no-build",
            "--when-idle",
            "--drain-timeout",
            "1",
        ],
    )
    .await;
    assert_eq!(code, 4, "a drain timeout has its own exit code\n{stderr}");
    assert!(stderr.contains("nothing was restarted"), "{stderr}");
    assert!(stdout.contains("waiting"), "{stdout}");

    assert!(pidfile::pid_alive(before.pid));
    let still = fetch_status(port).await.unwrap();
    assert_eq!(still.pid, before.pid);
    assert_eq!(still.mode, Mode::Play, "the mode was put back");

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The rule this file exists to pin down: an *upgrade* resumes the mode, a
/// cold start takes the default. Here is the cold start — a `play` server is
/// stopped and a fresh one is started over the same database, and it comes up
/// paused rather than quietly resuming dispatch on a machine nobody is
/// watching.
#[tokio::test]
async fn a_cold_start_after_a_playing_server_comes_up_paused() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    assert_eq!(before.mode, Mode::Pause, "the default on a fresh database");
    set_mode(port, Mode::Play).await;
    assert_eq!(mode(port).await, Mode::Play);

    cli(dir.path(), &["stop"]).await;
    wait_until_gone(before.pid).await;
    let _ = old.start_kill();

    // Not a reload: this is what a crash loop, a `launchd` KeepAlive or a
    // hand-typed `tasks serve` looks like.
    let (mut server, after) = start_server(dir.path(), port).await;
    assert_ne!(after.pid, before.pid);
    assert_eq!(
        after.mode,
        Mode::Pause,
        "starting a server must not be the same act as resuming dispatch"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// …and `TASKS_DEFAULT_MODE=play` is the honest way to ask a host to come back
/// dispatching, rather than re-reading the stored column.
#[tokio::test]
async fn a_configured_default_mode_is_what_a_boot_takes() {
    let dir = DataDir::new();
    let port = free_port().await;
    let mut server = serve_command(dir.path(), port)
        .env("TASKS_DEFAULT_MODE", "play")
        .spawn()
        .unwrap();
    let status = wait_serving(port).await;
    assert_eq!(status.mode, Mode::Play);

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// An upgrade is the one path that carries the mode: it travels in the child's
/// environment (so there is no window in which the new server runs in the
/// default) and is verified against the new pid before `reload` claims it.
#[tokio::test]
async fn an_upgrade_carries_the_mode_over() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("mode carried over: play"), "{stdout}");

    wait_until_gone(before.pid).await;
    let after = wait_serving(port).await;
    assert_ne!(after.pid, before.pid);
    assert_eq!(after.mode, Mode::Play, "an upgrade resumes the mode");

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// A mode that already matches the default is not a carry, and `reload` does
/// not claim one: a carry is an override, and one that agrees with the default
/// is noise.
#[tokio::test]
async fn a_swap_of_a_paused_server_carries_nothing() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    assert_eq!(before.mode, Mode::Pause);

    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stdout.contains("carried over"), "{stdout}");

    wait_until_gone(before.pid).await;
    assert_eq!(wait_serving(port).await.mode, Mode::Pause);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// An unusable `TASKS_DEFAULT_MODE` makes `serve` refuse to boot, so `reload`
/// resolves it as step 0 — before the build, and long before anything is
/// signalled. Finding out after the SIGTERM would turn a typo into an outage.
#[tokio::test]
async fn an_unusable_default_mode_is_refused_before_anything_is_signalled() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;

    let (code, stdout, stderr) =
        cli_with(dir.path(), &["reload", "--no-build"], Some("playing")).await;
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("TASKS_DEFAULT_MODE"), "{stderr}");
    assert!(stderr.contains("nothing was touched"), "{stderr}");
    assert!(
        !stdout.contains("stopping pid"),
        "the server must not be signalled: {stdout}"
    );
    assert!(pidfile::pid_alive(before.pid));
    assert_eq!(fetch_status(port).await.unwrap().pid, before.pid);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// Build first, so a compile error costs nothing. `--repo` points at an empty
/// directory: cargo fails immediately, with no compile to pay for.
#[tokio::test]
async fn a_failed_build_leaves_the_server_untouched() {
    let dir = DataDir::new();
    let empty = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;

    let (code, stdout, stderr) = cli(
        dir.path(),
        &["reload", "--repo", empty.path().to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("build failed"), "{stderr}");
    assert!(
        !stdout.contains("stopping pid"),
        "the server must not be signalled before a successful build: {stdout}"
    );
    assert!(pidfile::pid_alive(before.pid));
    assert_eq!(fetch_status(port).await.unwrap().pid, before.pid);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// Two servers on one data dir is the failure the pidfile exists to prevent —
/// and the refusal has to happen before the store is opened, or the refusing
/// process has already migrated the running server's database.
#[tokio::test]
async fn a_second_server_on_one_data_dir_refuses() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut first, before) = start_server(dir.path(), port).await;

    let other = free_port().await;
    let output = serve_command(dir.path(), other)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(stderr.contains("tasks reload"), "{stderr}");

    // The original is untouched and still owns the pidfile.
    assert_eq!(fetch_status(port).await.unwrap().pid, before.pid);
    assert_eq!(pidfile::read(dir.path()).unwrap().pid, before.pid);
    assert!(fetch_status(other).await.is_none());

    let _ = first.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The pidfile is a discovery record, not a lock: a record naming a pid that
/// is gone must never wedge a start. (procps' `kill -0` says an out-of-range
/// pid is alive, which is exactly how this would wedge forever.)
#[tokio::test]
async fn a_stale_pidfile_does_not_block_a_start() {
    let dir = DataDir::new();
    let port = free_port().await;
    let stale = serde_json::json!({
        "pid": 4_000_000_000u32,
        "port": port,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "exe": "/nonexistent/tasks",
    });
    std::fs::write(
        pidfile::path(dir.path()),
        serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();

    let (mut server, status) = start_server(dir.path(), port).await;
    assert_eq!(pidfile::read(dir.path()).unwrap().pid, status.pid);

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// `tasks stop` is a graceful SIGTERM that waits for the process to really be
/// gone — the handler `serve` did not have before this — and leaves no record
/// behind to clean up by hand.
#[tokio::test]
async fn stop_is_graceful_and_clears_the_pidfile() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;

    let (code, stdout, stderr) = cli(dir.path(), &["stop"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains(&format!("stopped pid {}", status.pid)),
        "{stdout}"
    );
    assert!(!pidfile::pid_alive(status.pid));
    assert!(pidfile::read(dir.path()).is_none(), "no record left behind");
    assert!(fetch_status(port).await.is_none());

    // A second stop is a no-op, not an error.
    let (code, stdout, _) = cli(dir.path(), &["stop"]).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("not serving"), "{stdout}");

    // And `status` says so, with an exit code a script can branch on.
    let (code, stdout, _) = cli(dir.path(), &["status"]).await;
    assert_eq!(code, 1);
    assert!(stdout.contains("not serving"), "{stdout}");

    let _ = server.start_kill();
}

/// `tasks stop --when-idle` waits on the *same* predicate
/// `reload --when-idle` waits on, and differs in exactly one lasting way:
/// there is no successor to hand the mode to, so dispatch stays paused — and
/// the command says so, with the undo.
#[tokio::test]
async fn stop_when_idle_waits_for_the_drain_and_leaves_dispatch_paused() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    let session = insert_running_session(dir.path()).await;

    let data_dir = dir.path().to_path_buf();
    let stop = tokio::spawn(async move {
        cli(
            &data_dir,
            &["stop", "--when-idle", "--drain-timeout", DRAIN_TIMEOUT],
        )
        .await
    });

    // It pauses before it waits — without which the wait never terminates.
    let mut paused = false;
    for _ in 0..100 {
        if mode(port).await == Mode::Pause {
            paused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(paused, "the drain must pause dispatch or it never ends");
    assert!(
        pidfile::pid_alive(status.pid),
        "nothing is signalled while it is still waiting"
    );

    // The scout lands; the stop follows.
    finish_session(dir.path(), &session).await;

    let (code, stdout, stderr) = stop.await.unwrap();
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("drained"), "{stdout}");
    assert!(stdout.contains("stays paused after the stop"), "{stdout}");
    assert!(
        stdout.contains(&format!("stopped pid {}", status.pid)),
        "{stdout}"
    );
    // The one lasting consequence is the last thing said, with its undo.
    assert!(stdout.contains("dispatch is left paused"), "{stdout}");
    assert!(stdout.contains("/mode"), "{stdout}");

    assert!(!pidfile::pid_alive(status.pid));
    assert!(pidfile::read(dir.path()).is_none(), "no record left behind");
    let store = Store::open(dir.path().join("tasks.db")).await.unwrap();
    assert_eq!(
        store.get_mode().await.unwrap(),
        Mode::Pause,
        "a stop leaves dispatch paused; nothing after it would unpause"
    );

    let _ = server.start_kill();
}

/// A wait that never happened must not have side effects: with nothing in
/// flight, `--when-idle` stops immediately and does not touch the mode.
#[tokio::test]
async fn an_idle_stop_when_idle_leaves_the_mode_alone() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, stdout, stderr) = cli(dir.path(), &["stop", "--when-idle"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("nothing in flight"), "{stdout}");
    assert!(!stdout.contains("paused dispatch"), "{stdout}");
    assert!(!stdout.contains("dispatch is left paused"), "{stdout}");
    assert!(!pidfile::pid_alive(status.pid));

    let store = Store::open(dir.path().join("tasks.db")).await.unwrap();
    assert_eq!(
        store.get_mode().await.unwrap(),
        Mode::Play,
        "a stop that did not wait must not leave the pipeline paused"
    );

    let _ = server.start_kill();
}

/// A stop drain that gives up stops nothing and puts the mode back — the same
/// contract `reload --when-idle` has, said in the stop's own words.
#[tokio::test]
async fn a_stop_drain_that_times_out_stops_nothing() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) =
        cli(dir.path(), &["stop", "--when-idle", "--drain-timeout", "1"]).await;
    assert_eq!(code, 4, "a drain timeout has its own exit code\n{stderr}");
    assert!(stderr.contains("nothing was stopped"), "{stderr}");
    assert!(stdout.contains("waiting"), "{stdout}");

    assert!(pidfile::pid_alive(status.pid), "nothing was stopped");
    let still = fetch_status(port).await.unwrap();
    assert_eq!(still.pid, status.pid);
    assert_eq!(still.mode, Mode::Play, "the mode was put back");

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// `--when-idle` against a live pid that will not say what is in flight cannot
/// know when idle arrives, so it refuses (3) and names the way through. The
/// pidfile points at this test process: alive, and answering nothing.
///
/// A bare `tempdir` rather than [`DataDir`] on purpose — `DataDir::drop`
/// SIGKILLs whatever the pidfile names.
#[tokio::test]
async fn a_stop_that_cannot_tell_when_idle_arrives_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let record = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "exe": tasks_bin(),
    });
    std::fs::write(
        pidfile::path(dir.path()),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();

    let (code, stdout, stderr) = cli(dir.path(), &["stop", "--when-idle"]).await;
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("not answering /status"), "{stderr}");
    assert!(stderr.contains("`tasks stop`"), "{stderr}");
    assert!(
        pidfile::pid_alive(std::process::id()),
        "a refusal must not signal anything"
    );
    assert!(
        pidfile::read(dir.path()).is_some(),
        "and must leave the record alone"
    );
}

/// The flags a stop does not have stay errors: `--force` and `--no-build` are
/// the other subcommand's, and a typo must not silently stop the server.
#[tokio::test]
async fn stop_rejects_the_flags_that_are_not_its_own() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;

    for flag in ["--force", "--no-build", "--when-idl"] {
        let (code, stdout, stderr) = cli(dir.path(), &["stop", flag]).await;
        assert_ne!(code, 0, "{flag}: stdout: {stdout}\nstderr: {stderr}");
        assert!(stderr.contains("unexpected argument"), "{flag}: {stderr}");
        assert!(pidfile::pid_alive(status.pid), "{flag} stopped the server");
    }

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// With nothing running, a reload is just a start — including on a fresh data
/// dir, where the first boot honestly reports applying every migration.
#[tokio::test]
async fn reload_with_nothing_running_is_just_a_start() {
    let dir = DataDir::new();
    let port = free_port().await;

    let (code, stdout, stderr) = cli(
        dir.path(),
        &["reload", "--no-build", "--port", &port.to_string()],
    )
    .await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("not serving"), "{stdout}");
    assert!(stdout.contains("migrations: applied"), "{stdout}");

    let status = wait_serving(port).await;
    assert_eq!(pidfile::read(dir.path()).unwrap().pid, status.pid);
    assert!(dir.path().join("serve.log").is_file(), "it logs somewhere");

    // `tasks status` reports the same server the swap just proved.
    let (code, stdout, _) = cli(dir.path(), &["status"]).await;
    assert_eq!(code, 0);
    assert!(stdout.contains(&format!("pid {}", status.pid)), "{stdout}");

    cli(dir.path(), &["stop"]).await;
}

// --- the maintenance drain ---

/// The deliverable: a pipeline that is quiesced *and stays that way*, so the
/// operator can restart vm-pool or rebuild the images. The pause is the point
/// here, not the tool — nothing follows a drain that could undo it.
#[tokio::test]
async fn a_drain_holds_dispatch_until_it_is_resumed() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    let session = insert_running_session(dir.path()).await;

    let data_dir = dir.path().to_path_buf();
    let drain =
        tokio::spawn(
            async move { cli(&data_dir, &["drain", "--drain-timeout", DRAIN_TIMEOUT]).await },
        );

    // It pauses before it waits — without which the wait never terminates.
    let mut paused = false;
    for _ in 0..100 {
        if mode(port).await == Mode::Pause {
            paused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(paused, "the drain must pause dispatch or it never ends");
    assert!(
        pidfile::pid_alive(status.pid),
        "a drain never signals the server: it has to be usable before a pool restart"
    );

    finish_session(dir.path(), &session).await;

    let (code, stdout, stderr) = drain.await.unwrap();
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("stays paused until `tasks resume`"),
        "{stdout}"
    );
    assert!(stdout.contains("drained"), "{stdout}");
    assert!(stdout.contains("quiesced"), "{stdout}");
    assert!(stdout.contains("make images"), "{stdout}");
    assert!(stdout.contains("tasks resume"), "{stdout}");

    // Still serving, and still held.
    assert!(pidfile::pid_alive(status.pid));
    assert_eq!(
        mode(port).await,
        Mode::Pause,
        "the hold outlives the command"
    );

    // The edge is on the feed, because the artifact it leaves behind is a
    // `pause` nothing can tell from any other.
    assert!(
        drain_notes(dir.path())
            .await
            .iter()
            .any(|n| n.contains("held for host maintenance")),
        "{:?}",
        drain_notes(dir.path()).await
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The deliberate inversion of `stop --when-idle`, which returns early without
/// touching the mode: an idle pipeline nobody holds starts a scout on the next
/// tick, straight into the pool that is about to go down.
#[tokio::test]
async fn a_drain_of_an_idle_pipeline_still_holds_it() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("paused dispatch"), "{stdout}");
    assert_eq!(
        mode(port).await,
        Mode::Pause,
        "an idle pipeline nobody holds dispatches on the next tick"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// A pipeline that is already not playing is left exactly as it is — `stop` is
/// tighter than `pause`, so "pausing" it would quietly turn intake back on —
/// and the closing words say which of the two happened.
#[tokio::test]
async fn a_drain_of_a_stopped_pipeline_changes_no_mode_and_says_so() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Stop).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("already not playing"), "{stdout}");
    assert_eq!(mode(port).await, Mode::Stop, "stop is tighter than pause");
    // Held all the same, and the feed says so.
    assert!(
        drain_notes(dir.path())
            .await
            .iter()
            .any(|n| n.contains("held for host maintenance")),
        "a drain that changed no mode still records why the pipeline is quiet"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// `--check` on the state `make images` is safe in: nothing in flight and
/// dispatch not playing. It touches neither the mode nor any run.
#[tokio::test]
async fn a_check_passes_on_a_quiesced_pipeline_and_touches_nothing() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    assert_eq!(
        mode(port).await,
        Mode::Pause,
        "the default on a fresh database"
    );

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--check"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("quiesced"), "{stdout}");
    // A check holds nothing, so it must not borrow the held drain's words.
    assert!(!stdout.contains("tasks resume"), "{stdout}");
    assert!(
        drain_notes(dir.path()).await.is_empty(),
        "a check records nothing"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The other half of the gate: work in flight is not quiesced, and the refusal
/// names the command that waits it out.
#[tokio::test]
async fn a_check_refuses_work_in_flight() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--check"]).await;
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("1 scout in flight"), "{stderr}");
    assert!(stderr.contains("tasks drain"), "{stderr}");
    assert_eq!(mode(port).await, Mode::Pause, "a check touches no mode");

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The important one. Nothing in flight is **not** enough: a *playing*
/// pipeline tops scouts up on the dispatcher's next tick, so a multi-minute
/// rebuild started here races it — and a scout that starts during a rebuild
/// starts in the old image, which is the staleness the update hold exists to
/// prevent and is the one case it cannot see.
#[tokio::test]
async fn a_check_refuses_a_playing_pipeline_with_nothing_in_flight() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--check"]).await;
    assert_eq!(
        code, 3,
        "a playing pipeline is not quiesced\n{stdout}{stderr}"
    );
    assert!(stderr.contains("in flight  nothing") || stdout.contains("in flight  nothing"));
    assert!(stderr.contains("tasks drain"), "{stderr}");
    assert!(stderr.contains("old image"), "{stderr}");
    assert_eq!(
        mode(port).await,
        Mode::Play,
        "a check refuses; it does not hold"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// …and it passes with nothing serving, or the `make images` gate would only
/// work on a host that happens to be running the server. No dispatcher means
/// nothing that can start a container.
#[tokio::test]
async fn a_check_passes_with_nothing_serving() {
    let dir = DataDir::new();

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--check"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("not serving"), "{stdout}");

    // And so does the drain proper — there is nothing to hold.
    let (code, stdout, stderr) = cli(dir.path(), &["drain"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("not serving"), "{stdout}");
}

/// A drain that gives up has quiesced nothing, and a no-op must not have side
/// effects — least of all one whose whole purpose is to be relied on before
/// something destructive.
#[tokio::test]
async fn a_drain_that_times_out_holds_nothing_and_puts_the_mode_back() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, status) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--drain-timeout", "1"]).await;
    assert_eq!(code, 4, "a drain timeout has its own exit code\n{stderr}");
    assert!(stderr.contains("not quiesced"), "{stderr}");
    assert!(stderr.contains("do not restart vm-pool"), "{stderr}");
    assert!(stdout.contains("waiting"), "{stdout}");
    assert!(
        !stdout.contains("make images"),
        "nothing was held: {stdout}"
    );

    assert!(pidfile::pid_alive(status.pid), "a drain signals nothing");
    assert_eq!(mode(port).await, Mode::Play, "the mode was put back");
    assert!(
        drain_notes(dir.path())
            .await
            .iter()
            .any(|n| n.contains("nothing is held")),
        "the unwind is an edge too: {:?}",
        drain_notes(dir.path()).await
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The one refusal that is not about the pipeline's state: a live pid that
/// will not answer cannot be waited on, and "quiesced" about a server we
/// cannot see into is the wrong direction to be wrong in.
///
/// A bare `tempdir` rather than [`DataDir`] on purpose — `DataDir::drop`
/// SIGKILLs whatever the pidfile names, and that is this test process.
#[tokio::test]
async fn a_drain_against_a_server_that_will_not_answer_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let record = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "exe": tasks_bin(),
    });
    std::fs::write(
        pidfile::path(dir.path()),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();

    for args in [vec!["drain"], vec!["drain", "--check"]] {
        let (code, stdout, stderr) = cli(dir.path(), &args).await;
        assert_eq!(code, 3, "{args:?}: stdout: {stdout}\nstderr: {stderr}");
        assert!(
            stderr.contains("not answering /status"),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains("Do not restart vm-pool or rebuild images yet"),
            "{args:?}: {stderr}"
        );
    }
    assert!(
        pidfile::pid_alive(std::process::id()),
        "a refusal must not signal anything"
    );
}

/// They say opposite things about whether anything is touched, so whichever
/// way it fell, half the people who typed both would get the opposite of what
/// they asked for.
#[tokio::test]
async fn check_and_cancel_scouts_together_are_refused() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, stdout, stderr) = cli(dir.path(), &["drain", "--check", "--cancel-scouts"]).await;
    assert_ne!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("contradict"), "{stderr}");
    assert_eq!(
        mode(port).await,
        Mode::Play,
        "a usage error touches nothing"
    );

    // And an unknown flag stays an error, rather than meaning "proceed".
    let (code, _, stderr) = cli(dir.path(), &["drain", "--cancel-builds"]).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("unexpected argument"), "{stderr}");

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// `--cancel-scouts` routes through the API — a durable `cancellations` row
/// the dispatcher following the run reads — rather than removing the VM (#876).
///
/// And it does **not** guarantee the drain point arrives: nothing is following
/// this session (vm-pool is unreachable in these tests, exactly as it would be
/// mid-restart), so the wait still runs out and this exits 4. That is the
/// honest answer — the drain promises the pipeline is quiesced, and it cannot
/// promise that about a VM nobody is watching.
#[tokio::test]
async fn cancel_scouts_records_the_request_and_still_waits_for_the_run() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;
    let session = insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(
        dir.path(),
        &["drain", "--cancel-scouts", "--drain-timeout", "1"],
    )
    .await;
    assert_eq!(code, 4, "nothing is following the run\n{stdout}{stderr}");
    assert!(stdout.contains("asked scout"), "{stdout}");
    assert!(stdout.contains(session.as_str()), "{stdout}");

    let store = Store::open(dir.path().join("tasks.db")).await.unwrap();
    let request = store
        .pending_cancel(RunKind::Session, session.as_str())
        .await
        .unwrap()
        .expect("a durable cancellation row");
    assert!(
        request
            .rationale
            .as_deref()
            .unwrap_or_default()
            .contains("maintenance"),
        "the rationale lands in the run's exit_reason: {request:?}"
    );
    assert_eq!(
        mode(port).await,
        Mode::Play,
        "the timeout put the mode back"
    );

    let _ = server.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// The undo, and the only thing that is: nothing resumes automatically,
/// because only the operator knows the pool is back and the images are built.
#[tokio::test]
async fn resume_releases_the_hold_and_reports_what_it_found() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut server, _) = start_server(dir.path(), port).await;
    set_mode(port, Mode::Play).await;

    let (code, _, stderr) = cli(dir.path(), &["drain"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(mode(port).await, Mode::Pause);

    let (code, stdout, stderr) = cli(dir.path(), &["resume"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("was pause"), "{stdout}");
    assert_eq!(mode(port).await, Mode::Play);
    assert!(
        drain_notes(dir.path())
            .await
            .iter()
            .any(|n| n.contains("hold is released")),
        "both edges are on the feed: {:?}",
        drain_notes(dir.path()).await
    );

    // An unknown argument is an error, and nothing serving is exit 1: there is
    // no mode to write, and claiming otherwise would report a hold released
    // that nothing is holding.
    let (code, _, stderr) = cli(dir.path(), &["resume", "--now"]).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("unexpected argument"), "{stderr}");

    cli(dir.path(), &["stop"]).await;
    let _ = server.start_kill();
    let (code, stdout, _) = cli(dir.path(), &["resume"]).await;
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("not serving"), "{stdout}");
}
