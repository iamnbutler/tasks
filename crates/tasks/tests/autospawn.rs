//! vm-pool autospawn, against the real binary.
//!
//! The property under test is end-to-end: `TASKS_VM_POOL_AUTOSPAWN=on`
//! reaches `Config::from_env`, the dispatch loop's failed connect spawns
//! `tasks vm-pool` from the serving binary, and the pool *binds* — a socket
//! that answers is the only evidence that matters, because everything short
//! of it (the spawn, the child surviving) can succeed while the pool refuses
//! or crashes. Only a real process can be asked whether it bound a socket,
//! which is the same argument `tests/cli.rs` makes.
//!
//! The same two env conventions as every other file that execs this binary:
//! per-test tempdirs for `TASKS_DATA_DIR` and `VM_POOL_SOCKET`, and
//! `TASKS_ENV_FILES=off` so a maintainer's gitignored `.env` cannot decide
//! the outcome. See `tests/reload.rs` for the long version.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::process::Command;

/// How long the pool gets to come up. Generous: on the far side of it is a
/// server that never spawned anything, and the failure message says so.
const POOL_WAIT: Duration = Duration::from_secs(30);

fn tasks_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tasks"))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// An orchestrator that costs nothing — without it, `ORCHESTRATOR_CMD`
/// defaults to `claude` and a nudge spawns a real agent turn.
fn stub_orchestrator(data_dir: &Path) -> PathBuf {
    let stub = data_dir.join("stub-orchestrator.sh");
    std::fs::write(&stub, "#!/bin/sh\ncat > /dev/null\necho ok\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    stub
}

/// tracing's fmt layer writes ANSI even into a file; the escapes carry
/// digits, so they have to go before anything numeric is read out of a line.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip to the terminating letter of a CSI sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The pid the server logged for its spawned pool — the handle the test needs
/// to clean up a daemon that deliberately outlives the server.
fn spawned_pool_pid(serve_log: &str) -> Option<u32> {
    for line in serve_log.lines() {
        let line = strip_ansi(line);
        if let Some(rest) = line.split("spawned vm-pool").nth(1) {
            let digits: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(pid) = digits.parse() {
                return Some(pid);
            }
        }
    }
    None
}

/// The whole flow: serve against a socket nothing serves, watch the pool the
/// server spawns come up and answer on it.
#[tokio::test]
async fn a_failed_connect_spawns_a_pool_that_binds() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let socket = data_dir.join("vm-pool.sock");
    let log_path = data_dir.join("serve-under-test.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let mut serve = Command::new(tasks_bin())
        .args(["serve", "--port", &free_port().to_string()])
        .env("TASKS_DATA_DIR", data_dir)
        .env("VM_POOL_SOCKET", &socket)
        .env("TASKS_VM_POOL_AUTOSPAWN", "on")
        .env("ORCHESTRATOR_CMD", stub_orchestrator(data_dir))
        .env("TASKS_BROKER_PORT", free_port().to_string())
        .env("TASKS_BROKER_BIND", "127.0.0.1")
        .env("TASKS_ENV_FILES", "off")
        .env_remove("GITHUB_TOKEN")
        .env_remove("TASKS_DEFAULT_MODE")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // The evidence: the socket accepts. Polled, because the pool is a real
    // process that has to boot.
    let deadline = tokio::time::Instant::now() + POOL_WAIT;
    let mut bound = false;
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(&socket).await.is_ok() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Clean up before asserting: the pool deliberately outlives the server
    // (own process group), so a failed assertion must not leak a daemon.
    let serve_log = std::fs::read_to_string(&log_path).unwrap_or_default();
    serve.kill().await.ok();
    let pool_pid = spawned_pool_pid(&serve_log);
    if let Some(pid) = pool_pid {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    assert!(
        bound,
        "no pool answered on {} within {POOL_WAIT:?}; serve log:\n{serve_log}",
        socket.display()
    );
    assert!(
        pool_pid.is_some(),
        "the socket answered but the server never logged `spawned vm-pool` — \
         who bound it? serve log:\n{serve_log}"
    );
}

/// The refusal path: garbage in `TASKS_VM_POOL_AUTOSPAWN` is a boot error
/// naming the variable, never a silently chosen default.
#[tokio::test]
async fn garbage_autospawn_refuses_to_boot() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("vm-pool.sock");
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(tasks_bin())
            .args(["serve", "--port", &free_port().to_string()])
            .env("TASKS_DATA_DIR", dir.path())
            .env("VM_POOL_SOCKET", &socket)
            .env("TASKS_VM_POOL_AUTOSPAWN", "maybe")
            .env("TASKS_ENV_FILES", "off")
            .env_remove("GITHUB_TOKEN")
            .output(),
    )
    .await
    .expect("a refused boot exits; it must not serve")
    .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TASKS_VM_POOL_AUTOSPAWN"),
        "the refusal should name the variable: {stderr}"
    );
    assert!(
        !socket.exists(),
        "a refused boot must not have spawned a pool"
    );
}
