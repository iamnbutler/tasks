//! `<data dir>/tasks.pid` — which server, on what port, from which binary.
//!
//! A *discovery record, not a lock*. Liveness is always re-derived from the OS
//! ([`pid_alive`]) and never believed from the file, so a server that was
//! killed leaves nothing to clean up by hand: the next start sees a pid that
//! is gone and overwrites the record. That is the same shape as `running`
//! session rows and startup reconciliation — the file is a hint, the world is
//! the authority.
//!
//! The record itself — its shape, its path, and how to read it — lives in
//! [`tasks_api::paths`], because clients read it too. What stays here is what
//! only a *local* process can answer: writing it, removing our own, and
//! whether a pid is alive.

use std::path::{Path, PathBuf};

use chrono::Utc;

pub use tasks_api::paths::{
    PID_FILE_NAME as FILE_NAME, PidFile, pid_file as path, read_pid_file as read,
};

/// Publish this process as the server under `data_dir`.
pub async fn write(data_dir: &Path, port: u16) -> std::io::Result<PidFile> {
    let file = PidFile {
        pid: std::process::id(),
        port,
        started_at: Utc::now(),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tasks")),
    };
    tokio::fs::create_dir_all(data_dir).await?;
    let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    tokio::fs::write(path(data_dir), json).await?;
    Ok(file)
}

/// The record, but only if the process it names is still alive. This is the
/// only question callers should ask: [`read`] alone cannot distinguish a
/// running server from the corpse of one.
pub fn read_live(data_dir: &Path) -> Option<PidFile> {
    read(data_dir).filter(|f| pid_alive(f.pid))
}

/// Remove the record if it is ours, so a slow exit cannot delete the file its
/// successor has already written.
pub fn remove_if_ours(data_dir: &Path, pid: u32) {
    if read(data_dir).is_some_and(|f| f.pid == pid) {
        let _ = std::fs::remove_file(path(data_dir));
    }
}

/// Whether `pid` names a live process.
///
/// `ps` is the authority whenever it answers at all, because `kill -0` gets
/// this wrong in both directions and both directions bite:
///
/// - it succeeds on a **zombie**, and the server we just SIGTERMed is
///   routinely an unreaped child of whoever started it — so a swap would wait
///   out its whole stop grace on a process that already exited cleanly, then
///   SIGKILL a corpse and report that it would not stop;
/// - procps' `kill -0` exits **0** for an out-of-range pid, so a stale pidfile
///   naming an impossible pid would make `serve` refuse to start forever.
///
/// `kill -0` is only the fallback for when `ps` could not be *run* at all,
/// because "I could not ask" must never read as "it is gone": the next action
/// on that answer is starting a second server against a live database.
pub fn pid_alive(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
    {
        Ok(output) => {
            let state = String::from_utf8_lossy(&output.stdout);
            let state = state.trim();
            // No row at all: no such process. A `Z` row: exited, not yet
            // reaped — dead for every purpose this crate has.
            !state.is_empty() && !state.starts_with('Z')
        }
        Err(_) => kill_zero(pid),
    }
}

/// `kill -0`, the fallback when `ps` is not runnable.
fn kill_zero(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let written = write(dir.path(), 4801).await.unwrap();
        let read_back = read(dir.path()).unwrap();
        assert_eq!(written, read_back);
        assert_eq!(read_back.pid, std::process::id());
        assert_eq!(read_back.port, 4801);
    }

    #[tokio::test]
    async fn remove_only_touches_our_own_record() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), 4800).await.unwrap();

        // A predecessor exiting late must not delete the successor's file.
        remove_if_ours(dir.path(), std::process::id() + 1);
        assert!(read(dir.path()).is_some());

        remove_if_ours(dir.path(), std::process::id());
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn own_pid_is_alive_and_an_impossible_one_is_not() {
        assert!(pid_alive(std::process::id()));
        // Above every pid_max in practice, and the case procps' `kill -0`
        // reports as alive.
        assert!(!pid_alive(4_000_000_000));
    }

    #[tokio::test]
    async fn read_live_ignores_a_stale_record() {
        let dir = tempfile::tempdir().unwrap();
        let stale = PidFile {
            pid: 4_000_000_000,
            port: 4800,
            started_at: Utc::now(),
            exe: PathBuf::from("/nonexistent/tasks"),
        };
        std::fs::write(path(dir.path()), serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(read(dir.path()).is_some());
        assert!(read_live(dir.path()).is_none());
    }

    /// The zombie case, which is the one that actually bit: a child that has
    /// exited but not been reaped is dead, whatever `kill -0` says.
    #[test]
    fn a_zombie_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        // Wait for it to exit without reaping it.
        for _ in 0..100 {
            let state = std::process::Command::new("ps")
                .args(["-o", "state=", "-p", &child.id().to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&state.stdout)
                .trim()
                .starts_with('Z')
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pid_alive(child.id()));
        let _ = child.wait();
    }
}
