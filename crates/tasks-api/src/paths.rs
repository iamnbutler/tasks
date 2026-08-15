//! Where a server publishes itself on disk: the data dir, the pidfile, the
//! serve log.
//!
//! Not a wire type, and deliberately here anyway. The server *writes*
//! `<data dir>/tasks.pid` and two separate clients read it — the CLI
//! (`reload` / `status` / `stop`) and the GUI's Server menu, which needs to
//! know which binary is serving before it can restart it. A private copy in
//! either would be a second definition of a record they compare, which is the
//! same argument that keeps the build stamp in one crate.
//!
//! Everything here is a *hint*. The record says a pid was published, never
//! that it is alive: liveness is re-derived from the OS by whoever asks, so a
//! killed server leaves nothing to clean up by hand. A corrupt or absent file
//! reads as "nobody published anything" rather than as an error, because the
//! next action on that answer must not be blocked by a file that failed to
//! parse.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Overrides the data dir — read by the server and by every client, so they
/// agree on which server they are talking about.
pub const DATA_DIR_ENV: &str = "TASKS_DATA_DIR";

/// The default data dir, relative to `$HOME`.
pub const DEFAULT_DATA_DIR: &str = ".local/state/tasks-v2";

/// The pidfile's name under the data dir.
pub const PID_FILE_NAME: &str = "tasks.pid";

/// Where a backgrounded `tasks serve` writes its log.
pub const SERVE_LOG_NAME: &str = "serve.log";

/// What a serving process publishes about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub port: u16,
    pub started_at: DateTime<Utc>,
    /// The binary that is serving — the fact that makes "did my new build
    /// actually take over?" answerable without a `ps` puzzle, and the fact
    /// that lets a GUI restart the right binary without a `PATH` guess.
    pub exe: PathBuf,
}

/// `$TASKS_DATA_DIR`, else `$HOME/.local/state/tasks-v2`.
///
/// `None` only when neither is set — a homeless environment, which the caller
/// turns into whatever its own "cannot proceed" is.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var(DATA_DIR_ENV).ok().filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_DATA_DIR))
}

/// `<data dir>/tasks.pid`.
pub fn pid_file(data_dir: &Path) -> PathBuf {
    data_dir.join(PID_FILE_NAME)
}

/// `<data dir>/serve.log`.
pub fn serve_log(data_dir: &Path) -> PathBuf {
    data_dir.join(SERVE_LOG_NAME)
}

/// The record, if there is a parseable one. See the module docs: this says a
/// pid was published, not that it is alive.
pub fn read_pid_file(data_dir: &Path) -> Option<PidFile> {
    let raw = std::fs::read_to_string(pid_file(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) {
        std::fs::write(pid_file(dir), body).unwrap();
    }

    #[test]
    fn a_published_record_reads_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = PidFile {
            pid: 4242,
            port: 4800,
            started_at: Utc::now(),
            exe: PathBuf::from("/usr/local/bin/tasks"),
        };
        write(dir.path(), &serde_json::to_string(&file).unwrap());
        assert_eq!(read_pid_file(dir.path()), Some(file));
    }

    /// A hint that fails to parse is not an error anyone should act on.
    #[test]
    fn missing_and_corrupt_files_read_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pid_file(dir.path()).is_none());
        write(dir.path(), "not json");
        assert!(read_pid_file(dir.path()).is_none());
    }

    #[test]
    fn paths_hang_off_the_data_dir() {
        let dir = Path::new("/state/tasks-v2");
        assert_eq!(pid_file(dir), PathBuf::from("/state/tasks-v2/tasks.pid"));
        assert_eq!(serve_log(dir), PathBuf::from("/state/tasks-v2/serve.log"));
    }
}
