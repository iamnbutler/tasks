//! Argument handling, against the real binary.
//!
//! In-process assertions would be cheaper, but the property under test is
//! **"no side effect"** — asking for help must not start a daemon — and only a
//! real process can be asked whether it bound a socket. So every test here
//! execs `tasks` and then looks at the filesystem it was pointed at.
//!
//! Two settings are forced on every child, and both matter.
//! `TASKS_DATA_DIR` and `VM_POOL_SOCKET` are per-test tempdirs, so nothing
//! here can touch the developer's real `/tmp/vm-pool.sock` or their store —
//! the whole point of `tasks vm-pool --help` starting a daemon is that it did
//! so against whatever socket the environment named. And `TASKS_ENV_FILES=off`,
//! for the reason `tests/reload.rs` spells out at length: `env_remove` is the
//! *opposite* of a scrub, because `.env` is what a removed variable is handed
//! to, and `.env` is gitignored — so without the switch a maintainer with
//! `TASKS_DEFAULT_MODE=play` in one fails this suite on their machine and
//! nowhere else. Any future test file that execs this binary needs the same.
//!
//! Every run is wrapped in a timeout that fails as an assertion rather than
//! hanging until nextest kills the test with no output — the failure mode
//! being guarded against is precisely "it started a server instead of
//! printing".

use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;

/// A run that exceeds this has not printed usage; it has started something.
const RUN_TIMEOUT: Duration = Duration::from_secs(20);

fn tasks_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tasks"))
}

/// One `tasks` invocation in its own data dir, with its own socket path.
/// Returns the output and the directory, so a caller can assert about what was
/// (not) created.
async fn run(args: &[&str]) -> (Output, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("vm-pool.sock");
    let output = tokio::time::timeout(
        RUN_TIMEOUT,
        Command::new(tasks_bin())
            .args(args)
            .env("TASKS_DATA_DIR", dir.path())
            .env("VM_POOL_SOCKET", &socket)
            .env("TASKS_ENV_FILES", "off")
            .output(),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("`tasks {}` did not finish in {RUN_TIMEOUT:?} — it started something rather than printing", args.join(" "))
    })
    .unwrap();
    (output, dir)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The defect: `vm_pool()` took no arguments at all, so an unrecognized one
/// meant "proceed" and `--help` started the daemon. The exit status is the
/// least of it — the assertions that matter are the two paths that must not
/// exist afterwards.
#[tokio::test]
async fn vm_pool_help_prints_usage_and_starts_nothing() {
    let (output, dir) = run(&["vm-pool", "--help"]).await;
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("usage: tasks vm-pool"), "{text}");
    assert!(
        text.contains("REFUSES to start"),
        "the usage should say it is a daemon that refuses an occupied socket: {text}"
    );

    assert!(
        !dir.path().join("vm-pool.sock").exists(),
        "asking for help bound the socket"
    );
    assert!(
        !dir.path().join("snapshots").exists(),
        "asking for help created the snapshot store"
    );
}

/// `-h` reaches the same place. Same assertion about side effects, because
/// "one spelling was fixed" is the shape of the original bug.
#[tokio::test]
async fn vm_pool_dash_h_prints_usage_and_starts_nothing() {
    let (output, dir) = run(&["vm-pool", "-h"]).await;
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("usage: tasks vm-pool"));
    assert!(!dir.path().join("vm-pool.sock").exists());
    assert!(!dir.path().join("snapshots").exists());
}

/// The general form of the rule: an unrecognized argument never means
/// "proceed". It exits non-zero and names the thing it did not understand.
#[tokio::test]
async fn an_unknown_flag_is_refused_and_named() {
    let (output, dir) = run(&["vm-pool", "--danger"]).await;
    assert!(!output.status.success(), "an unknown flag must not proceed");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(text.contains("--danger"), "{text}");
    assert!(!dir.path().join("vm-pool.sock").exists());
}

/// Every subcommand answers, including the alias — there is no command whose
/// `--help` falls through to "start whatever this is".
#[tokio::test]
async fn every_subcommand_answers_help() {
    for (command, expected) in [
        ("serve", "usage: tasks serve"),
        ("reload", "usage: tasks reload"),
        ("restart", "usage: tasks reload"),
        ("status", "usage: tasks status"),
        ("stop", "usage: tasks stop"),
        ("doctor", "usage: tasks doctor"),
        ("add-project", "usage: tasks add-project"),
        ("vm-pool", "usage: tasks vm-pool"),
    ] {
        let (output, _dir) = run(&[command, "--help"]).await;
        assert!(
            output.status.success(),
            "`tasks {command} --help`: {output:?}"
        );
        let text = stdout(&output);
        assert!(
            text.contains(expected),
            "`tasks {command} --help` should print {expected:?}: {text}"
        );
    }
}

/// The top-level help still answers "what commands are there" — the
/// per-command texts are an addition to it, not a replacement.
#[tokio::test]
async fn top_level_help_still_lists_the_subcommands() {
    let (output, _dir) = run(&["--help"]).await;
    assert!(output.status.success());
    let text = stdout(&output);
    for command in [
        "serve",
        "reload",
        "status",
        "stop",
        "doctor",
        "add-project",
        "vm-pool",
    ] {
        assert!(text.contains(command), "{command} missing from: {text}");
    }
}

/// It used to take `args.first()` and drop the rest in silence, so a second
/// repo was tracked nowhere and reported nowhere.
#[tokio::test]
async fn add_project_refuses_a_second_repo() {
    let (output, _dir) = run(&["add-project", "a/b", "c/d"]).await;
    assert!(!output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(text.contains("c/d"), "the extra argument is named: {text}");
}

/// The incident, through the CLI: a second daemon against a live socket
/// refuses, and — the half that matters — the incumbent is still reachable
/// through the path afterwards. The old code left it listening on an unlinked
/// inode: alive, `pgrep`-able, and permanently unreachable.
#[tokio::test]
async fn a_second_vm_pool_refuses_a_live_socket() {
    use tokio::net::{UnixListener, UnixStream};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("vm-pool.sock");
    // Stands in for a running daemon: what `bind_socket` probes is whether
    // anything answers a connect on the path, which the kernel does out of the
    // listen backlog.
    let incumbent = UnixListener::bind(&socket).unwrap();

    let output = tokio::time::timeout(
        RUN_TIMEOUT,
        Command::new(tasks_bin())
            .arg("vm-pool")
            .env("TASKS_DATA_DIR", dir.path())
            .env("VM_POOL_SOCKET", &socket)
            .env("TASKS_ENV_FILES", "off")
            .output(),
    )
    .await
    .expect("a refusal returns immediately; a takeover would run forever")
    .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        text.contains("refusing to start a second one"),
        "the refusal should say what it refused: {text}"
    );
    assert!(text.contains(&socket.display().to_string()), "{text}");

    UnixStream::connect(&socket)
        .await
        .expect("the incumbent still owns the path");
    drop(incumbent);
}

/// `tasks doctor` on a machine with nothing set up: it has to *answer*, and
/// it has to leave the data dir it was pointed at as it found it.
///
/// The exit code is not asserted to be 1 here — a host that happens to have
/// every precondition would legitimately pass — but the report's shape is,
/// because "the same shape every time" is the property that lets a reader tell
/// "not asked" from "not present".
#[tokio::test]
async fn doctor_reports_without_writing_anything() {
    let (output, dir) = run(&["doctor"]).await;
    let text = stdout(&output);

    for section in [
        "environment",
        "configuration",
        "container runtime",
        "vm-pool",
        "server",
        "VM images",
        "credentials",
        "credential broker",
        "github",
        "projects",
        "orchestrator",
    ] {
        assert!(text.contains(section), "{section} missing from:\n{text}");
    }
    // Nothing short-circuits: a question that could not be asked says so
    // rather than being omitted.
    assert!(text.contains("skip"), "{text}");

    // The write probe is the one deliberate write, and it cleans up after
    // itself; nothing else here may create a file at all. In particular the
    // store is never opened, so no `tasks.db` and no migrations.
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        left.is_empty(),
        "doctor left files in the data dir it was pointed at: {left:?}"
    );
}

/// Every failing check names the command that changes it — asserted through
/// the real binary, not just at the constructor.
#[tokio::test]
async fn doctor_names_a_fix_beside_every_complaint() {
    let (output, _dir) = run(&["doctor"]).await;
    let text = stdout(&output);
    let complaints = text.lines().filter(|l| l.contains("FAIL ")).count();
    let fixes = text
        .lines()
        .filter(|l| l.trim_start().starts_with("-> "))
        .count();
    assert!(
        complaints > 0,
        "expected a bare tempdir to fail something:\n{text}"
    );
    assert!(
        fixes >= complaints,
        "{complaints} failure(s) but only {fixes} fix line(s):\n{text}"
    );
}

/// A usage error is 2, kept apart from the 1 a real failure gets, so a setup
/// script can tell "your machine is broken" from "you typed it wrong".
#[tokio::test]
async fn doctor_exits_two_on_an_unknown_flag() {
    let (output, _dir) = run(&["doctor", "--fix"]).await;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(text.contains("--fix"), "{text}");
}
