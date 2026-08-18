//! `tasks service`, against the real binary — the *safe* half.
//!
//! What is deliberately not here: an end-to-end `install`. `launchctl`
//! registers against the real per-user launchd domain no matter what `$HOME`
//! says, so a test that bootstrapped would leave a live agent named
//! `com.iamnbutler.tasks.server` in the developer's session — the exact kind
//! of side effect `tests/cli.rs` exists to prove commands do not have. The
//! pieces an install is made of (the plist rendering, the copy-and-rename,
//! the delegation guard) are covered as unit tests in `src/service.rs`;
//! what this file pins is that the refusal paths refuse *before* touching
//! launchd, with `$HOME` pointed at a tempdir so "nothing installed" is a
//! fact about the test and not about the machine.
//!
//! Same env conventions as every file that execs the binary: per-test
//! tempdirs for `TASKS_DATA_DIR` and `HOME`, and `TASKS_ENV_FILES=off`.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;

const RUN_TIMEOUT: Duration = Duration::from_secs(20);

fn tasks_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tasks"))
}

/// One `tasks service …` invocation with `$HOME` and the data dir in their
/// own tempdir, so what is (not) installed is the test's fact alone.
async fn run(args: &[&str]) -> (Output, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let output = tokio::time::timeout(
        RUN_TIMEOUT,
        Command::new(tasks_bin())
            .args(args)
            .env("HOME", dir.path())
            .env("TASKS_DATA_DIR", dir.path().join("data"))
            .env("TASKS_ENV_FILES", "off")
            .output(),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "`tasks {}` did not finish in {RUN_TIMEOUT:?}",
            args.join(" ")
        )
    })
    .unwrap();
    (output, dir)
}

#[tokio::test]
async fn status_with_nothing_installed_says_so_and_exits_zero() {
    let (output, dir) = run(&["service", "status"]).await;
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("not installed"), "{text}");
    assert!(text.contains("not serving"), "{text}");
    // Saying so must not have created anything.
    assert!(
        !dir.path()
            .join("Library/LaunchAgents/com.iamnbutler.tasks.server.plist")
            .exists()
    );
    assert!(!dir.path().join(".tasks").exists());
}

/// `start`, `stop` and `restart` all need an installed agent, and the
/// refusal has to name the fix — and happen before any launchctl call, which
/// is what the tempdir `$HOME` proves: there is nothing here launchd could
/// have been asked about.
#[tokio::test]
async fn lifecycle_verbs_refuse_without_an_install_and_name_the_fix() {
    for verb in ["start", "stop", "restart"] {
        let (output, _dir) = run(&["service", verb]).await;
        assert!(
            !output.status.success(),
            "`service {verb}` should refuse with nothing installed"
        );
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            text.contains("tasks service install"),
            "`service {verb}` refusal should name the fix: {text}"
        );
    }
}

#[tokio::test]
async fn service_help_prints_usage_and_starts_nothing() {
    let (output, dir) = run(&["service", "--help"]).await;
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("usage: tasks service"), "{text}");
    assert!(!dir.path().join(".tasks").exists());
}

/// An unknown verb is an error naming the usage, never a default action.
#[tokio::test]
async fn an_unknown_verb_is_refused() {
    let (output, _dir) = run(&["service", "installl"]).await;
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("unknown service subcommand"), "{text}");
}
