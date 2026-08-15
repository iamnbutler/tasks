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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tasks::models::{
    GhState, Mode, Project, ProjectId, Session, SessionId, SessionStatus, Task, TaskId, TaskState,
};
use tasks::pidfile;
use tasks::store::Store;
use tasks_api::http::ServerStatus;
use tempfile::TempDir;
use tokio::process::Command;

fn tasks_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tasks"))
}

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
        .env_remove("GITHUB_TOKEN")
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
    let output = Command::new(tasks_bin())
        .args(args)
        .env("TASKS_DATA_DIR", data_dir)
        .env("VM_POOL_SOCKET", data_dir.join("vm-pool.sock"))
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
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

/// The happy path: one command, and a different process is serving the same
/// port from the same data dir — with the schema question answered by the new
/// process rather than assumed.
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

/// The default gate: a 20-minute scout is not destroyed because someone typed
/// `reload`, and the refusal names both ways forward.
#[tokio::test]
async fn a_scout_in_flight_refuses_the_swap_until_forced() {
    let dir = DataDir::new();
    let port = free_port().await;
    let (mut old, before) = start_server(dir.path(), port).await;
    insert_running_session(dir.path()).await;

    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build"]).await;
    assert_eq!(code, 3, "busy has its own exit code\n{stdout}{stderr}");
    assert!(stderr.contains("1 scout in flight"), "{stderr}");
    assert!(stderr.contains("--when-idle"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    // The report showed it, with its age, before refusing.
    assert!(stdout.contains("scout"), "{stdout}");
    assert!(
        pidfile::pid_alive(before.pid),
        "a refusal must not touch the server"
    );
    assert_eq!(fetch_status(port).await.unwrap().pid, before.pid);

    // Told twice, it swaps.
    let (code, stdout, stderr) = cli(dir.path(), &["reload", "--no-build", "--force"]).await;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    wait_until_gone(before.pid).await;
    let after = wait_serving(port).await;
    assert_ne!(after.pid, before.pid);
    // The new boot reconciled the scout it interrupted.
    assert!(after.in_flight.scouts.is_empty(), "{:?}", after.in_flight);

    let _ = old.start_kill();
    cli(dir.path(), &["stop"]).await;
}

/// `--when-idle` pauses dispatch (without which the wait never terminates),
/// waits for the work to land, swaps, and puts the mode back — after the new
/// server answers, never before.
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
                "60",
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
