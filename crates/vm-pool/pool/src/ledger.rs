//! What this pool started, written down where its successor can read it.
//!
//! A pool's map of VMs lives in memory, so a daemon that dies — killed,
//! crashed, or restarted by a human — takes with it the only record of the VMs
//! it started. Those VMs keep running: nothing inside them notices, and
//! nothing outside them is looking. The next daemon comes up believing the
//! host is empty, and the containers are left for a person with `container
//! ls` and a shell.
//!
//! The ledger is that record. [`VmLedger::record`] is called **before** the VM
//! is started and [`VmLedger::forget`] after it is stopped, so the file is a
//! superset of what is running rather than a subset — over-stopping is
//! idempotent, under-stopping is the leak.
//!
//! # Why a written record rather than an inventory
//!
//! The obvious alternative is to ask the runtime what it is running (`container
//! ls`) and stop whatever this pool does not recognise. Two things make that
//! wrong, and either alone is fatal:
//!
//! 1. **A VM name carries no daemon identity.** Ids are `vm-<micros>-<counter>`
//!    and nothing in them says which pool started them, while pointing a second
//!    pool at another `VM_POOL_SOCKET` is a configuration this project
//!    explicitly suggests — it is in `BindError::AlreadyRunning`'s own message.
//!    So a sweep over unrecognised `vm-*` names would tear down a *live* peer's
//!    VMs: the wrong-takeover that `bind_socket` exists to prevent, arriving
//!    through a different door.
//! 2. **The parser would be unrunnable where it is written.** apple/container
//!    is macOS-only, so the one load-bearing line of the fix could never be
//!    executed by a test, by CI, or by an agent in a Linux VM.
//!
//! This never asks what exists; it remembers what this pool started. Its safety
//! is a proof rather than a heuristic: `bind_socket` admits exactly one live
//! daemon per socket path, and the file is named for that path, so everything
//! in it at boot belongs to a daemon that is gone.
//!
//! # Failure is degradation, never refusal
//!
//! No method returns an error. A ledger that cannot be read or written is a
//! pool that leaks VMs the way it always did; refusing to boot over one would
//! trade a recoverable leak for an outage. An unparseable file is moved aside
//! rather than deleted — while it cannot be acted on, it is still the only
//! record of what may be running.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vm_pool_protocol::VmId;

/// The on-disk shape. `socket` is for whoever opens the file in an editor —
/// the file name is sanitized and hashed, and a human reading it should not
/// have to reverse that to learn which pool it belongs to.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerFile {
    socket: String,
    vms: Vec<String>,
}

/// The VM ids this pool started, persisted across the pool's own death.
///
/// See the [module docs](self) for why this exists and why it is a written
/// record rather than an inventory of the host.
#[derive(Debug)]
pub struct VmLedger {
    /// `None` is a fully working ledger that forgets everything — for tests
    /// and for embedders with nowhere to write.
    path: Option<PathBuf>,
    socket: String,
    ids: Mutex<BTreeSet<String>>,
}

impl VmLedger {
    /// Open the ledger for `socket_path`, returning it together with the VMs a
    /// previous daemon on that socket left running.
    ///
    /// The carried-over ids stay *in* the ledger until
    /// [`Pool::reclaim_carried_over`](crate::Pool::reclaim_carried_over) has
    /// stopped them, so a daemon that dies partway through the cleanup hands
    /// the remainder to the next one.
    pub fn open(state_dir: &Path, socket_path: &Path) -> (Self, Vec<VmId>) {
        let socket = socket_path.display().to_string();
        if let Err(e) = std::fs::create_dir_all(state_dir) {
            warn!(
                dir = %state_dir.display(),
                error = %e,
                "could not create the VM ledger's directory; this pool will not \
                 remember its VMs across a restart"
            );
            return (Self::disabled(), Vec::new());
        }

        let path = state_dir.join(file_name_for(socket_path));
        let carried = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<LedgerFile>(&raw) {
                Ok(file) => file.vms,
                Err(e) => {
                    // Kept, not deleted: it cannot be acted on, and it is
                    // still the only record of what may be running.
                    let aside = path.with_extension("json.unreadable");
                    let moved = std::fs::rename(&path, &aside).is_ok();
                    warn!(
                        path = %path.display(),
                        error = %e,
                        moved_to = moved.then(|| aside.display().to_string()),
                        "the VM ledger could not be read; any VMs a previous \
                         pool left running must be stopped by hand"
                    );
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "could not open the VM ledger; any VMs a previous pool left \
                     running must be stopped by hand"
                );
                Vec::new()
            }
        };

        if !carried.is_empty() {
            info!(
                path = %path.display(),
                count = carried.len(),
                "a previous pool on this socket left VMs running"
            );
        }

        let ledger = Self {
            path: Some(path),
            socket,
            ids: Mutex::new(carried.iter().cloned().collect()),
        };
        (ledger, carried.into_iter().map(VmId::new).collect())
    }

    /// A ledger with nowhere to write: every method works, nothing is
    /// persisted, and nothing is ever carried over.
    pub fn disabled() -> Self {
        Self {
            path: None,
            socket: String::new(),
            ids: Mutex::new(BTreeSet::new()),
        }
    }

    /// Where this ledger writes, if anywhere.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Remember a VM. Called **before** the runtime starts it: recording
    /// afterwards would lose exactly the VM whose daemon died between the
    /// spawn and the write, which is the crash this exists for.
    pub fn record(&self, vm_id: &VmId) {
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        if !ids.insert(vm_id.as_str().to_string()) {
            return;
        }
        let snapshot: Vec<String> = ids.iter().cloned().collect();
        drop(ids);
        self.persist(snapshot);
    }

    /// Forget a VM that has been stopped, or that never started.
    pub fn forget(&self, vm_id: &VmId) {
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        if !ids.remove(vm_id.as_str()) {
            return;
        }
        let snapshot: Vec<String> = ids.iter().cloned().collect();
        drop(ids);
        self.persist(snapshot);
    }

    /// What this ledger currently holds.
    pub fn ids(&self) -> Vec<VmId> {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(VmId::new)
            .collect()
    }

    /// Temp file plus rename, so a torn write can never be what the successor
    /// reads. A failure is a warning: the caller is in the middle of starting
    /// or stopping a VM, and neither should fail because a bookkeeping file
    /// could not be written.
    fn persist(&self, vms: Vec<String>) {
        let Some(path) = &self.path else {
            return;
        };
        let file = LedgerFile {
            socket: self.socket.clone(),
            vms,
        };
        let Ok(json) = serde_json::to_vec_pretty(&file) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!(path = %tmp.display(), error = %e, "could not write the VM ledger");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            warn!(path = %path.display(), error = %e, "could not replace the VM ledger");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The ledger's file name for a socket path.
///
/// Two pools on one host share a state directory — [`ServiceConfig`]'s default
/// derives it from the user, not from the socket — so the socket has to reach
/// the file name, or each pool would read the other's VMs at boot and stop
/// them while they were live.
///
/// A sanitized path alone is not enough: `/tmp/a.sock` and `/tmp/a-sock` both
/// sanitize to `tmp-a-sock`, and a collision here is exactly the mistake the
/// ledger must not make. So the readable part is a hint and the hash is the
/// identity. The hash is FNV-1a rather than [`std::hash::DefaultHasher`]
/// because a file name has to mean the same thing after a Rust upgrade, and
/// `DefaultHasher`'s output explicitly does not.
///
/// [`ServiceConfig`]: https://docs.rs/vm-pool-service
fn file_name_for(socket_path: &Path) -> String {
    let raw = socket_path.display().to_string();
    let mut hint: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    hint = hint.trim_matches('-').to_string();
    hint.truncate(64);
    format!("vms-{hint}-{:016x}.json", fnv1a(raw.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_hands_its_vms_to_the_next_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = Path::new("/tmp/vm-pool.sock");

        let (ledger, carried) = VmLedger::open(dir.path(), socket);
        assert!(carried.is_empty(), "nothing ran before this");
        ledger.record(&VmId::new("vm-1"));
        ledger.record(&VmId::new("vm-2"));
        ledger.forget(&VmId::new("vm-1"));
        drop(ledger);

        let (_next, carried) = VmLedger::open(dir.path(), socket);
        assert_eq!(carried, vec![VmId::new("vm-2")]);
    }

    /// The carried ids stay in the file until they are forgotten, so a daemon
    /// that dies partway through the cleanup hands the rest on.
    #[test]
    fn carried_over_vms_survive_until_they_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let socket = Path::new("/tmp/vm-pool.sock");

        let (first, _) = VmLedger::open(dir.path(), socket);
        first.record(&VmId::new("vm-1"));
        first.record(&VmId::new("vm-2"));
        drop(first);

        let (second, carried) = VmLedger::open(dir.path(), socket);
        assert_eq!(carried.len(), 2);
        second.forget(&VmId::new("vm-1"));
        drop(second);

        let (_third, carried) = VmLedger::open(dir.path(), socket);
        assert_eq!(
            carried,
            vec![VmId::new("vm-2")],
            "the one that was never stopped is still owed"
        );
    }

    /// The whole point of keying the file on the socket: two pools on one host
    /// share a state directory, and each must see only its own VMs.
    #[test]
    fn two_sockets_never_share_a_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, _) = VmLedger::open(dir.path(), Path::new("/tmp/mine.sock"));
        let (theirs, _) = VmLedger::open(dir.path(), Path::new("/tmp/theirs.sock"));
        assert_ne!(mine.path(), theirs.path());

        mine.record(&VmId::new("vm-mine"));
        theirs.record(&VmId::new("vm-theirs"));

        let (_, carried) = VmLedger::open(dir.path(), Path::new("/tmp/mine.sock"));
        assert_eq!(carried, vec![VmId::new("vm-mine")], "not the peer's VM");
    }

    /// Sanitizing alone collides, and a collision here means stopping a live
    /// peer's VMs.
    #[test]
    fn paths_that_sanitize_alike_still_get_their_own_file() {
        assert_ne!(
            file_name_for(Path::new("/tmp/a.sock")),
            file_name_for(Path::new("/tmp/a-sock")),
        );
        assert_eq!(
            file_name_for(Path::new("/tmp/vm-pool.sock")),
            file_name_for(Path::new("/tmp/vm-pool.sock")),
            "the same socket is the same file, run after run"
        );
    }

    #[test]
    fn an_unreadable_ledger_is_moved_aside_rather_than_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = Path::new("/tmp/vm-pool.sock");
        let path = dir.path().join(file_name_for(socket));
        std::fs::write(&path, "{ this is not json").unwrap();

        let (ledger, carried) = VmLedger::open(dir.path(), socket);
        assert!(carried.is_empty(), "nothing readable to carry");
        let aside = path.with_extension("json.unreadable");
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ this is not json",
            "the only record of what may be running is kept"
        );

        // And it still works from here.
        ledger.record(&VmId::new("vm-1"));
        let (_, carried) = VmLedger::open(dir.path(), socket);
        assert_eq!(carried, vec![VmId::new("vm-1")]);
    }

    /// A directory that cannot be created degrades to a working ledger that
    /// persists nothing — never to a refusal.
    #[test]
    fn nowhere_to_write_is_a_working_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("state");
        std::fs::write(&blocker, "not a directory").unwrap();

        let (ledger, carried) = VmLedger::open(&blocker, Path::new("/tmp/vm-pool.sock"));
        assert!(carried.is_empty());
        assert!(ledger.path().is_none());
        ledger.record(&VmId::new("vm-1"));
        assert_eq!(ledger.ids(), vec![VmId::new("vm-1")]);

        let disabled = VmLedger::disabled();
        disabled.record(&VmId::new("vm-2"));
        disabled.forget(&VmId::new("vm-2"));
        assert!(disabled.path().is_none());
    }
}
