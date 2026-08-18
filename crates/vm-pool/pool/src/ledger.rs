//! What this pool started, remembered across the daemon's own death.
//!
//! `container run` outlives the process that spawned it. A pool that is killed
//! — or that panics, or whose host restarts it — therefore leaves every VM it
//! held running, owned by nobody: an **orphan**. Nothing in the pool's own
//! memory survives to find them, and the health loop cannot: it only ages out
//! VMs that are still in *this* process's map.
//!
//! So the ledger is a write-ahead record of the VM ids this pool started, on
//! disk, named for the socket it listens on. The next daemon to bind that
//! socket reads it and asks the runtime to stop whatever the last one left.
//!
//! # Why a file and not `container ls`
//!
//! Two independent reasons, either one fatal:
//!
//! 1. **VM names carry no daemon identity** (`vm-<micros>-<counter>`), and
//!    pointing a second pool at another `VM_POOL_SOCKET` is a configuration
//!    this project suggests in `BindError::AlreadyRunning`'s own message. A
//!    sweep that stopped every unrecognised `vm-*` would tear down a *live
//!    peer's* VMs — the wrong-takeover that `bind_socket` exists to prevent,
//!    arriving through a different door.
//! 2. **apple/container is macOS-only**, so the parser would be the one
//!    load-bearing line of this fix that no test or CI run on Linux could
//!    ever execute.
//!
//! The ledger never asks what exists. It remembers what this pool started.
//!
//! # What forgetting an id costs, and what earns it
//!
//! Recovery is only ever as strong as the runtime's `stop`, so `stop` answers
//! a **verdict**: `Ok(())` is the claim that the VM is not running, and
//! [`crate::PoolError::StopFailed`] is "could not confirm". An id is forgotten
//! only on the first, by both callers — [`crate::Pool::deallocate`] and
//! [`crate::Pool::reclaim_carried_over`]. That is what forgetting costs: it is
//! the deletion of the only record that a container exists, so it has to be
//! earned by an answer rather than by an attempt.
//!
//! An id that is not forgotten is asked again by the *next* daemon on this
//! socket, and the one after that. A container that never stops is therefore
//! kept forever — the behaviour, not a leak, since the alternative is dropping
//! the record — and costs one `container stop` per boot plus one warning
//! naming it, which is also how anyone finds out.
//!
//! A reported success is still the runtime's word. `ContainerRuntime` trusts
//! `container stop`'s exit 0 rather than verifying the container died, so on
//! that path "the successor asked the runtime to stop it" is the honest verb.
//! What is new is that a **refusal** is retried across boots.
//!
//! What *is* recoverable on every runtime is an **interrupted** reclaim, and
//! only because [`VmLedger::enable`] **seeds** the in-memory set with the
//! carried ids. Each [`VmLedger::forget`] then persists the remainder, so a
//! daemon that dies partway through the loop hands whatever it did not reach
//! to the next one. Without that seeding the first `record` or `forget` would
//! rewrite the file from an empty set and erase every carried id at once,
//! stopped or not.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;
use vm_pool_protocol::VmId;

/// The on-disk shape. An object rather than a bare array so a later field can
/// be added without the old file becoming unparseable — which, per
/// [`VmLedger::enable`], is a quarantine rather than a no-op.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    vms: Vec<VmId>,
}

/// A durable set of the VM ids this pool started.
///
/// Construct one [`disabled`](VmLedger::disabled) — that is what every
/// embedder that never runs a socket gets, including the whole test suite, so
/// no test writes into a real state directory. [`VmLedger::enable`] gives it a
/// file and returns what the previous daemon on that socket left behind.
pub struct VmLedger {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    /// `None` is a disabled ledger: it asserts nothing and writes nothing.
    path: Option<PathBuf>,
    /// What the file asserts. Seeded from the file at `enable`, which is what
    /// keeps a persist from dropping the carried ids.
    ids: HashSet<VmId>,
}

impl Default for VmLedger {
    fn default() -> Self {
        Self::disabled()
    }
}

impl VmLedger {
    /// A fully working ledger with no file. `record` and `forget` are no-ops,
    /// and [`outstanding`](VmLedger::outstanding) is empty.
    pub fn disabled() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Point this ledger at `state_dir`'s file for `socket_path`, and return
    /// the ids the previous daemon on that socket left behind.
    ///
    /// **Reads and never writes** — not even `create_dir_all`, which happens
    /// at the first persist — so a caller that decides not to act on the
    /// result leaves the predecessor's file byte-for-byte as it found it. That
    /// guarantee does not depend on this being inert *in memory*; it depends
    /// on `enable` never being *called*, which is what the service's
    /// bind-then-adopt ordering gives.
    ///
    /// It is deliberately not read-only in memory: the carried ids are seeded
    /// into this ledger's own set, so the first `record` or `forget` persists
    /// the remainder rather than an empty file.
    pub async fn enable(&self, state_dir: &Path, socket_path: &Path) -> Vec<VmId> {
        let path = state_dir.join(file_name_for(socket_path));

        let carried = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<LedgerFile>(&bytes) {
                Ok(file) => file.vms,
                Err(e) => {
                    // Never deleted: it is the only record that those VMs
                    // exist. If it cannot even be moved aside, stay disabled
                    // rather than overwrite it.
                    let quarantine = path.with_extension("json.unreadable");
                    match std::fs::rename(&path, &quarantine) {
                        Ok(()) => {
                            warn!(
                                path = %path.display(),
                                moved_to = %quarantine.display(),
                                error = %e,
                                "the VM ledger is unreadable; moved it aside rather than \
                                 deleting it — the VMs it names may still be running"
                            );
                            Vec::new()
                        }
                        Err(rename_error) => {
                            warn!(
                                path = %path.display(),
                                error = %e,
                                %rename_error,
                                "the VM ledger is unreadable and could not be moved aside; \
                                 running without one rather than overwriting it"
                            );
                            return Vec::new();
                        }
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                // Unreadable for a reason that is not absence (permissions, a
                // directory in the way). Enabling would let a later persist
                // overwrite a file this process could not read.
                warn!(
                    path = %path.display(),
                    error = %e,
                    "could not read the VM ledger; running without one"
                );
                return Vec::new();
            }
        };

        let mut seen: HashSet<VmId> = HashSet::new();
        let carried: Vec<VmId> = carried
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();

        let mut inner = self.inner.write().await;
        inner.path = Some(path);
        inner.ids = seen;
        carried
    }

    /// Remember `vm_id` as this pool's.
    ///
    /// Called **write-ahead** — before the VM is started — because recording
    /// afterwards loses exactly the VM whose daemon died between the spawn and
    /// the write, which is the crash window the ledger exists for.
    pub async fn record(&self, vm_id: &VmId) {
        let mut inner = self.inner.write().await;
        // A disabled ledger returns before touching even the in-memory set:
        // that set means "what the file asserts", and a ledger with no file
        // asserts nothing.
        if inner.path.is_none() {
            return;
        }
        if inner.ids.insert(vm_id.clone()) {
            inner.persist();
        }
    }

    /// Forget `vm_id` — this pool handed it back, or a successor asked the
    /// runtime to stop it.
    pub async fn forget(&self, vm_id: &VmId) {
        let mut inner = self.inner.write().await;
        if inner.path.is_none() {
            return;
        }
        if inner.ids.remove(vm_id) {
            inner.persist();
        }
    }

    /// What the file currently asserts, sorted. Diagnostics — and what lets a
    /// test assert that construction adopted nothing.
    pub async fn outstanding(&self) -> Vec<VmId> {
        let inner = self.inner.read().await;
        sorted(&inner.ids)
    }

    /// The file this ledger writes, if it has one.
    pub async fn path(&self) -> Option<PathBuf> {
        self.inner.read().await.path.clone()
    }
}

impl Inner {
    /// Write the set out, temp-file-and-rename.
    ///
    /// Nothing here returns an error. A pool that refused to boot over an
    /// unwritable state directory would trade a recoverable memory leak for an
    /// outage, and one that refused to *allocate* over it would be worse. So
    /// every failure is a `warn!` and a ledger that records nothing.
    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                path = %parent.display(),
                error = %e,
                "could not create the VM ledger's directory; this pool's VMs will not be \
                 reclaimed by its successor"
            );
            return;
        }

        let file = LedgerFile {
            vms: sorted(&self.ids),
        };
        let json = match serde_json::to_vec_pretty(&file) {
            Ok(json) => json,
            Err(e) => {
                warn!(error = %e, "could not serialize the VM ledger");
                return;
            }
        };

        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!(
                path = %tmp.display(),
                error = %e,
                "could not write the VM ledger; this pool's VMs will not be reclaimed by \
                 its successor"
            );
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            warn!(
                path = %path.display(),
                error = %e,
                "could not replace the VM ledger; this pool's VMs will not be reclaimed by \
                 its successor"
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn sorted(ids: &HashSet<VmId>) -> Vec<VmId> {
    let mut out: Vec<VmId> = ids.iter().cloned().collect();
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out
}

/// `vms-<sanitized tail of the socket path>-<fnv1a hex>.json`.
///
/// The readable half is for the human who finds the file. The **hash is what
/// makes it correct**: sanitizing is lossy (`/tmp/a.sock` and `/tmp/a-sock`
/// flatten to the same string), and two pools on one host share a state
/// directory, so one ledger between them would have each stopping the other's
/// live VMs at boot.
fn file_name_for(socket_path: &Path) -> String {
    let tail = socket_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sanitized: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "socket".to_string()
    } else {
        sanitized
    };
    let hash = fnv1a(socket_path.to_string_lossy().as_bytes());
    format!("vms-{sanitized}-{hash:016x}.json")
}

/// FNV-1a, hand-rolled: no dependency, and — unlike `DefaultHasher` — stable
/// across builds, which a file name read back at the next boot has to be.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<VmId> {
        v.iter().map(|s| VmId::new(*s)).collect()
    }

    #[tokio::test]
    async fn a_disabled_ledger_writes_nothing_and_asserts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = VmLedger::disabled();
        ledger.record(&VmId::new("vm-1")).await;
        ledger.forget(&VmId::new("vm-1")).await;
        assert!(ledger.outstanding().await.is_empty());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a ledger with no file must not create one"
        );
    }

    #[tokio::test]
    async fn what_one_daemon_records_the_next_one_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let first = VmLedger::disabled();
        assert!(first.enable(dir.path(), &socket).await.is_empty());
        first.record(&VmId::new("vm-a")).await;
        first.record(&VmId::new("vm-b")).await;

        let second = VmLedger::disabled();
        assert_eq!(
            second.enable(dir.path(), &socket).await,
            ids(&["vm-a", "vm-b"])
        );
    }

    #[tokio::test]
    async fn a_vm_this_pool_handed_back_is_not_carried() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let first = VmLedger::disabled();
        first.enable(dir.path(), &socket).await;
        first.record(&VmId::new("vm-a")).await;
        first.record(&VmId::new("vm-b")).await;
        first.forget(&VmId::new("vm-a")).await;

        let second = VmLedger::disabled();
        assert_eq!(second.enable(dir.path(), &socket).await, ids(&["vm-b"]));
    }

    /// Required item (1): without seeding the in-memory set at `enable`, this
    /// first `record` rewrites the file from an empty set and every carried id
    /// vanishes at that moment — stopped or not.
    #[tokio::test]
    async fn the_first_record_does_not_erase_the_carried_ids() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let first = VmLedger::disabled();
        first.enable(dir.path(), &socket).await;
        first.record(&VmId::new("vm-old-1")).await;
        first.record(&VmId::new("vm-old-2")).await;

        let second = VmLedger::disabled();
        let carried = second.enable(dir.path(), &socket).await;
        assert_eq!(carried, ids(&["vm-old-1", "vm-old-2"]));
        // The successor allocates before it reclaims — the order the bug
        // actually took.
        second.record(&VmId::new("vm-new")).await;

        let third = VmLedger::disabled();
        assert_eq!(
            third.enable(dir.path(), &socket).await,
            ids(&["vm-new", "vm-old-1", "vm-old-2"])
        );
    }

    /// The interrupted reclaim, at the unit level: one carried id is stopped
    /// and forgotten, and the rest are still on disk for the next boot.
    #[tokio::test]
    async fn forgetting_one_carried_id_leaves_the_rest_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");

        let first = VmLedger::disabled();
        first.enable(dir.path(), &socket).await;
        for id in ["vm-1", "vm-2", "vm-3"] {
            first.record(&VmId::new(id)).await;
        }

        let second = VmLedger::disabled();
        assert_eq!(second.enable(dir.path(), &socket).await.len(), 3);
        second.forget(&VmId::new("vm-1")).await;
        drop(second); // the daemon dies partway through the loop

        let third = VmLedger::disabled();
        assert_eq!(
            third.enable(dir.path(), &socket).await,
            ids(&["vm-2", "vm-3"])
        );
    }

    #[tokio::test]
    async fn two_sockets_in_one_state_directory_get_two_ledgers() {
        let dir = tempfile::tempdir().unwrap();
        let a = VmLedger::disabled();
        let b = VmLedger::disabled();
        a.enable(dir.path(), &dir.path().join("a.sock")).await;
        b.enable(dir.path(), &dir.path().join("b.sock")).await;
        a.record(&VmId::new("vm-a")).await;
        b.record(&VmId::new("vm-b")).await;

        assert_ne!(a.path().await, b.path().await);
        let reread = VmLedger::disabled();
        assert_eq!(
            reread.enable(dir.path(), &dir.path().join("a.sock")).await,
            ids(&["vm-a"]),
            "one pool must never read the other's VMs as its own to stop"
        );
    }

    /// Sanitizing is lossy, so the hash is what keeps two paths apart.
    #[test]
    fn paths_that_sanitize_alike_still_get_different_files() {
        assert_ne!(
            file_name_for(Path::new("/tmp/a.sock")),
            file_name_for(Path::new("/tmp/a-sock")),
        );
        assert_ne!(
            file_name_for(Path::new("/tmp/one/vm-pool.sock")),
            file_name_for(Path::new("/tmp/two/vm-pool.sock")),
        );
        assert_eq!(
            file_name_for(Path::new("/tmp/vm-pool.sock")),
            file_name_for(Path::new("/tmp/vm-pool.sock")),
            "the name has to be stable across builds, not just within one"
        );
    }

    #[tokio::test]
    async fn an_unparseable_ledger_is_moved_aside_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vm-pool.sock");
        let path = dir.path().join(file_name_for(&socket));
        std::fs::write(&path, "{ this is not the ledger").unwrap();

        let ledger = VmLedger::disabled();
        assert!(ledger.enable(dir.path(), &socket).await.is_empty());

        let quarantine = path.with_extension("json.unreadable");
        assert_eq!(
            std::fs::read_to_string(&quarantine).unwrap(),
            "{ this is not the ledger",
            "the file is the only record those VMs exist"
        );
        // And the ledger is usable afterwards rather than wedged.
        ledger.record(&VmId::new("vm-new")).await;
        let next = VmLedger::disabled();
        assert_eq!(next.enable(dir.path(), &socket).await, ids(&["vm-new"]));
    }
}
