//! How big the orchestrator's warm verification build directory is, and what
//! bounds it.
//!
//! `ORCHESTRATOR_TARGET_DIR` is a `CARGO_TARGET_DIR` the orchestrator builds
//! and tests in, shared and long-lived because the warmth is the whole reason
//! a merge decision can rest on a real test run rather than on a typecheck.
//! It had no bound and — worse — no *report*: CLAUDE.md said "expect ~7.5 GB
//! and nothing prunes it", and it was found at 39 GB by a human hunting for
//! disk (#1010), then measured at **51 GB** a week later, growing ~2 GB per
//! verification.
//!
//! **The growth was never the warmth.** Cargo keys an artifact on a metadata
//! hash that includes the source path, and the prompt sent the agent into a
//! *fresh* `git worktree` per verification — so each new worktree path added a
//! complete new set of workspace artifacts and the previous set was kept
//! forever. Registry dependencies, whose hash does not include the workspace
//! path, are shared across all of them and were never what grew. That is why
//! the source-side fixes (one reused worktree named in the prompt,
//! `CARGO_INCREMENTAL=0` on the child) matter more than anything in this
//! module: the reclaim here is the backstop, not the mechanism.
//!
//! Measured on the real directory, **2026-08-20**, at 51 GB total:
//!
//! ```text
//!    35.24 GB   208468 files  .o          (codegen units, 6141 over 1 MB)
//!     6.14 GB      226 files  executables (test/bin)
//!     3.75 GB     1168 files  .rlib
//!     1.46 GB     1718 files  .rmeta
//!     0.18 GB       52 files  .dylib
//!    46.79 GB                 deps/ total
//!    24.24 GB                 incremental/
//! ```
//!
//! Three things follow from those numbers, and they are recorded here so the
//! next person does not re-measure. **`incremental/` alone is 24 GB of 51**, so
//! tier 1 below is not a token gesture — on that host it does the whole job.
//! **75% of `deps/` is codegen-unit object files**, which is macOS's default
//! `split-debuginfo = "unpacked"` for the dev profile (this workspace declares
//! no `[profile]` section, so it takes it) leaving the debuginfo *beside* the
//! linked binaries rather than inside them. And an eviction tier that removed
//! only the linked executables — considered, and now measured and **wrong** —
//! would free 6.14 GB, 13% of `deps/`, while leaving all 35 GB of the
//! debuginfo it was aimed at. It is not implemented and is not recorded as a
//! future refinement.
//!
//! **mtime-based eviction is actively backwards here**, and should not be
//! "improved" into this later. Registry dependency artifacts are built once at
//! the very start and never touched again, so they are always the *oldest*
//! files: a `cargo-sweep --time`-shaped policy deletes exactly the warmth that
//! is load-bearing and keeps the per-worktree garbage. The stamp-file variant
//! does not rescue it either — a no-op `cargo build` touches nothing, so
//! "older than the stamp" means everything.
//!
//! **The reclaim is permissible here in a way it is not for a rejected
//! bundle** ([`crate::bundles`]): everything in this directory is reproducible
//! from the checkout, so a deletion costs time and never work. The one cost
//! that must not be paid quietly is that the wholesale tier makes the next
//! verification cold, and a verification that does not finish inside a turn
//! leaves carve-out (b) undischarged — the batch goes to a human. So a reclaim
//! is announced on the event feed and stays on `/status` for the rest of the
//! boot.
//!
//! Shape notes, each with a reason:
//!
//! - The [`Reading`] is **in memory and never a table**, the
//!   [`crate::github_health`]/[`crate::pool_health`] rule: it is a fact about
//!   a filesystem with a timestamp on it.
//! - It is refreshed on a **15-minute cadence rather than at read time**,
//!   because the walk is hundreds of thousands of files — 213,628 in `deps/`
//!   alone on the measured host. The orchestrator loop awaits it, so the first
//!   pass after a boot delays that tick by one walk; that is the price of not
//!   doing it on a route every client polls, and it is bounded by one walk
//!   rather than by how often anybody looks.
//! - **Hardlinks are counted once** (cargo hardlinks `<profile>/<bin>` to
//!   `<profile>/deps/<bin>-<hash>`), so the number agrees with the `du -sh`
//!   that found the problem.
//! - [`VerifyDir::measure_due`] claims the cadence on the **attempt**, not on a
//!   successful reading. Claiming only on success re-walks a directory that
//!   cannot be read on every tick of a loop that ticks once a second, with a
//!   `warn!` each time.
//! - The module never touches the store: [`VerifyDir::maintain`] returns at
//!   most one [`Notice`] and the caller appends the event — the
//!   [`crate::pool_health::Transition`] shape.
//! - A `Note`, **not an obligation**. Obligations go to the orchestrator, which
//!   is the one actor that must not be asked to manage this directory: it
//!   builds in it. Same argument that kept `ObligationKind::StaleImage` from
//!   existing.
//! - A reclaim is `Actor::System` maintenance of a local cache with **no
//!   charter gate**, like `run::reclaim_bundles`. It is not an agent action and
//!   not a GitHub write.
//!
//! Where it runs is the safety argument, and it lives in
//! [`crate::run::maintain_verify_dir`]: the orchestrator loop is the only thing
//! that starts a process in this directory, so calling this before each
//! `tick()` means a deletion cannot race a compile by construction rather than
//! by a lock. While the session is checked out interactively a human may be
//! building in it, so that case measures and reports but reclaims nothing.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use tasks_api::http::{VerifyDirReclaim, VerifyDirTier, VerifyDirUsage};

/// How often the directory is re-walked. Not a read-time measurement: the walk
/// is hundreds of thousands of files, and `/status` is polled by every client.
pub const MEASURE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Bytes in a gigabyte, as the budget is written (`ORCHESTRATOR_TARGET_BUDGET_GB`).
///
/// Decimal, not binary: the number this is compared against is the one `du -sh`
/// and every disk-space dialog show, and the whole point of the report is that
/// it agrees with what a human ran.
const BYTES_PER_GB: u64 = 1_000_000_000;

/// What one walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub bytes: u64,
    /// Files counted, hardlinks once. Reported because "51 GB" and "51 GB in
    /// 213,628 files" suggest different next questions.
    pub files: u64,
    pub at: DateTime<Utc>,
}

/// Which tier a reclaim reached — and, because the tiers are ordered by what
/// they cost, how expensive the next verification is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Every `<profile>/incremental`. Keyed to one worktree path, so a later
    /// checkout could never have reused it: this costs no warmth at all.
    ///
    /// With `CARGO_INCREMENTAL=0` now on the child this mostly clears what was
    /// accumulated before that shipped — 24.24 GB of 51 on the measured host —
    /// rather than something the pipeline keeps making. That is the intended
    /// steady state: the source-side fixes are the mechanism, this is the
    /// backstop.
    Incremental,
    /// The directory's contents. The next verification is cold — minutes
    /// before a single test runs — which is the cost that must not be paid
    /// quietly.
    Wholesale,
}

impl Tier {
    fn wire(self) -> VerifyDirTier {
        match self {
            Tier::Incremental => VerifyDirTier::Incremental,
            Tier::Wholesale => VerifyDirTier::Wholesale,
        }
    }
}

/// A reclaim that happened, kept for the rest of the boot.
///
/// Every number in it is **measured rather than estimated**: each tier
/// re-walks, so `after` is a reading and not `before` minus a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaim {
    pub at: DateTime<Utc>,
    pub tier: Tier,
    pub before: u64,
    pub after: u64,
    /// The ceiling that was exceeded, so the sentence can state both numbers.
    pub budget: u64,
}

impl Reclaim {
    /// The sentence the event feed gets.
    ///
    /// The wholesale tier names its cost in the same breath as the reclaim,
    /// because a cold verification is what routes the next batch to a human and
    /// nothing downstream would say why.
    pub fn describe(&self, dir: &Path) -> String {
        let freed = self.before.saturating_sub(self.after);
        let head = format!(
            "reclaimed {} from the orchestrator's verification build directory ({}): \
             {} over the {} ceiling, now {}",
            humanize_bytes(freed),
            dir.display(),
            humanize_bytes(self.before),
            humanize_bytes(self.budget),
            humanize_bytes(self.after),
        );
        match self.tier {
            Tier::Incremental => format!(
                "{head}. Only the per-profile `incremental` caches went, which are keyed \
                 to one worktree path and cost no warmth — the next verification is as \
                 warm as it was"
            ),
            Tier::Wholesale => format!(
                "{head}. Emptying the incremental caches was not enough, so the whole \
                 directory went: THE NEXT VERIFICATION IS COLD, which is minutes of \
                 compilation before a single test runs. A verification that does not \
                 finish inside one turn leaves the merge carve-out undischarged, so the \
                 batch it was for goes to a human"
            ),
        }
    }

    fn wire(&self) -> VerifyDirReclaim {
        VerifyDirReclaim {
            at: self.at,
            tier: self.tier.wire(),
            before_bytes: self.before,
            after_bytes: self.after,
        }
    }
}

/// What one [`VerifyDir::maintain`] pass has to say — at most one thing, and
/// nothing at all in the ordinary case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A reclaim happened.
    Reclaimed(Reclaim),
    /// The directory is over its ceiling and nothing reclaimed it, because a
    /// human has the orchestrator session checked out and may be building in
    /// here. (`ORCHESTRATOR_TARGET_BUDGET_GB=0` sets no ceiling at all, so
    /// nothing can be over it — that case reports and says nothing.)
    /// Announced **once per edge**, for
    /// [`crate::pool_health`]'s reason: a standing complaint every fifteen
    /// minutes is one a reader learns to skip.
    OverBudget { reading: Reading, budget: u64 },
}

/// The size of the orchestrator's verification build directory, as last
/// measured, and the ceiling past which it is reclaimed.
#[derive(Debug)]
pub struct VerifyDir {
    dir: PathBuf,
    /// The ceiling in bytes, or `None` for `ORCHESTRATOR_TARGET_BUDGET_GB=0` —
    /// report only, the `TASKS_UPDATE_HOLD=off` shape. **The report half is
    /// deliberately not switchable**: a directory that grows silently is
    /// exactly what #1010 was.
    budget: Option<u64>,
    /// Who may touch the directory right now. Read half: a run that builds in
    /// it (a worker run holds it for its whole job). Write half: the reclaim.
    ///
    /// This lock exists because the worker lane (#1053) broke the argument
    /// that used to make a lock unnecessary — "the orchestrator loop is the
    /// only thing that starts a process in this directory, so a deletion
    /// cannot race a compile by construction". With two lanes building here,
    /// construction no longer covers it, and a deletion racing a compile is
    /// exactly the failure the old argument existed to rule out. The reclaim
    /// takes `try_write` and **skips rather than waits** when the lane is
    /// busy: a worker holds the read half for up to its whole budget, and a
    /// maintenance pass parked behind it would stall the loop that runs it.
    lane: tokio::sync::RwLock<()>,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    reading: Option<Reading>,
    /// When a walk was last *claimed* — not when it answered. See the module
    /// doc: claiming on the attempt is what keeps an unreadable directory from
    /// being re-walked every tick.
    measured_at: Option<DateTime<Utc>>,
    /// Kept for the rest of the boot, so the cost of a wholesale reclaim is
    /// still on `/status` for whoever arrives after the feed has scrolled.
    last_reclaim: Option<Reclaim>,
    over_announced: bool,
}

impl VerifyDir {
    /// `budget_gb` of `0` means report only.
    pub fn new(dir: PathBuf, budget_gb: u64) -> Self {
        Self::with_budget(dir, (budget_gb > 0).then(|| budget_gb * BYTES_PER_GB))
    }

    /// The same thing in bytes, so the tests can put a ceiling under a few
    /// kilobytes of fixture rather than writing gigabytes to prove a
    /// comparison.
    pub fn with_budget(dir: PathBuf, budget: Option<u64>) -> Self {
        Self {
            dir,
            budget,
            lane: tokio::sync::RwLock::new(()),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Hold the directory against the reclaim for the duration of a build.
    ///
    /// Shared, not exclusive — the worker lane is serial anyway, and what
    /// this guards against is only the deletion. Await is fine here: the
    /// write half is only ever taken with `try_write`, so this never parks
    /// behind a reclaim in progress for longer than the deletion itself.
    pub async fn share(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.lane.read().await
    }

    /// Claim the next walk, if one is due. Claims on the **attempt**.
    pub fn measure_due(&self, now: DateTime<Utc>) -> bool {
        let mut inner = self.inner.lock().expect("verify dir lock");
        let due = match inner.measured_at {
            // Signed, and here that means a clock which stepped backwards
            // *waits* rather than walking — deliberately the opposite reading
            // to [`crate::pool_health::PoolHealth::probe_due`], because the two
            // are protecting different things. A probe there is one local unix
            // round trip and the risk is a stale hold; a walk here is hundreds
            // of thousands of `stat`s on a loop that ticks once a second, and
            // re-walking on every tick is precisely what the cadence exists to
            // prevent. The cost is bounded by the size of the step.
            Some(last) => now - last >= interval(),
            None => true,
        };
        if due {
            inner.measured_at = Some(now);
        }
        due
    }

    /// Walk if due, reclaim if over the ceiling and permitted to, and say
    /// whatever is worth saying — at most one thing.
    ///
    /// `may_reclaim` is `false` while a human has the orchestrator session
    /// checked out: they may be building in here, and this is the one case the
    /// "nothing else starts a process in this directory" argument does not
    /// cover.
    pub async fn maintain(&self, may_reclaim: bool) -> Option<Notice> {
        let now = Utc::now();
        if !self.measure_due(now) {
            return None;
        }
        let reading = self.walk(now).await?;
        let budget = self.budget?;
        if reading.bytes <= budget {
            self.inner.lock().expect("verify dir lock").over_announced = false;
            return None;
        }
        if !may_reclaim {
            return self.announce_over_budget(reading, budget);
        }
        // A run building in the directory holds the read half; deleting under
        // it is the race this lock exists for. Skip, not wait — the lane can
        // stay busy for a whole worker budget, and the next pass retries.
        let Ok(_lane) = self.lane.try_write() else {
            return self.announce_over_budget(reading, budget);
        };

        // Tier 1: the per-profile incremental caches. Keyed to one worktree
        // path, so nothing that survives could ever have reused them.
        let before = reading.bytes;
        let dir = self.dir.clone();
        let removed = tokio::task::spawn_blocking(move || remove_incremental(&dir))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
        if let Err(e) = removed {
            warn!(dir = %self.dir.display(), error = %e, "could not clear the incremental caches");
        }
        // Re-walked, not subtracted: every number reported here is measured.
        let after_tier1 = self.walk(Utc::now()).await?;
        if after_tier1.bytes <= budget {
            return Some(self.record(Reclaim {
                at: Utc::now(),
                tier: Tier::Incremental,
                before,
                after: after_tier1.bytes,
                budget,
            }));
        }

        // Tier 2: the contents, and only the contents. The directory itself is
        // created once per boot precisely so the prompt cannot name one the
        // agent will find missing — `remove_dir_all` on it would undo that
        // until the next restart.
        let dir = self.dir.clone();
        let emptied = tokio::task::spawn_blocking(move || empty_dir(&dir))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
        if let Err(e) = emptied {
            warn!(dir = %self.dir.display(), error = %e, "could not empty the build directory");
        }
        let after = self.walk(Utc::now()).await?;
        Some(self.record(Reclaim {
            at: Utc::now(),
            tier: Tier::Wholesale,
            before,
            after: after.bytes,
            budget,
        }))
    }

    /// Say the directory is over budget and not being reclaimed — once per
    /// stretch over the ceiling, however many passes decline.
    fn announce_over_budget(&self, reading: Reading, budget: u64) -> Option<Notice> {
        let mut inner = self.inner.lock().expect("verify dir lock");
        if std::mem::replace(&mut inner.over_announced, true) {
            return None;
        }
        drop(inner);
        Some(Notice::OverBudget { reading, budget })
    }

    /// What `/status` reports. `None` until the first successful walk —
    /// "nothing measured yet" rather than a zero that reads like an empty
    /// directory.
    pub fn usage(&self) -> Option<VerifyDirUsage> {
        let inner = self.inner.lock().expect("verify dir lock");
        let reading = inner.reading.as_ref()?;
        Some(VerifyDirUsage {
            path: self.dir.display().to_string(),
            bytes: reading.bytes,
            files: reading.files,
            measured_at: reading.at,
            budget_bytes: self.budget,
            over_budget: self.budget.is_some_and(|b| reading.bytes > b),
            last_reclaim: inner.last_reclaim.as_ref().map(Reclaim::wire),
        })
    }

    /// One walk, off the runtime's blocking pool — hundreds of thousands of
    /// `stat`s is not something to do on a reactor thread.
    ///
    /// `None` on a directory that cannot be read: the cadence is already
    /// claimed, so this is one `warn!` every fifteen minutes rather than one
    /// per tick.
    async fn walk(&self, now: DateTime<Utc>) -> Option<Reading> {
        let dir = self.dir.clone();
        let walked = tokio::task::spawn_blocking(move || measure(&dir)).await;
        let (bytes, files) = match walked {
            Ok(Ok(found)) => found,
            Ok(Err(e)) => {
                warn!(
                    dir = %self.dir.display(),
                    error = %e,
                    "could not measure the orchestrator's verification build directory"
                );
                return None;
            }
            Err(e) => {
                warn!(dir = %self.dir.display(), error = %e, "the build directory walk panicked");
                return None;
            }
        };
        let reading = Reading {
            bytes,
            files,
            at: now,
        };
        self.inner.lock().expect("verify dir lock").reading = Some(reading.clone());
        Some(reading)
    }

    fn record(&self, reclaim: Reclaim) -> Notice {
        let mut inner = self.inner.lock().expect("verify dir lock");
        inner.last_reclaim = Some(reclaim.clone());
        // A reclaim answers the complaint, so the next crossing is a fresh
        // edge worth announcing again.
        inner.over_announced = false;
        drop(inner);
        info!(
            dir = %self.dir.display(),
            before = reclaim.before,
            after = reclaim.after,
            "reclaimed the orchestrator's verification build directory"
        );
        Notice::Reclaimed(reclaim)
    }
}

fn interval() -> chrono::Duration {
    chrono::Duration::from_std(MEASURE_INTERVAL).expect("measure interval fits")
}

/// Bytes and files under `root`, **counting a hardlinked file once**.
///
/// Cargo hardlinks `<profile>/<bin>` to `<profile>/deps/<bin>-<hash>`, so
/// counting both would put this report above the `du -sh` a human runs to check
/// it — and a reporting tool that disagrees with `du` is one nobody trusts a
/// second time.
///
/// Symlinks are not followed (`symlink_metadata`), so a `target` directory
/// somebody pointed elsewhere is not silently counted twice or walked out of.
/// Unreadable entries below the root are skipped rather than failing the walk:
/// a partial number is worth more than none, and the root itself failing is
/// still an error.
fn measure(root: &Path) -> io::Result<(u64, u64)> {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    // Fails on the root only — everything below is skipped rather than fatal.
    let _ = fs::read_dir(root)?;
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                    continue;
                }
            }
            bytes += meta.len();
            files += 1;
        }
    }
    Ok((bytes, files))
}

/// Remove every `<profile>/incremental` under `root`.
///
/// Depth-limited to where cargo actually puts them — `<root>/debug/incremental`
/// and `<root>/<target-triple>/debug/incremental` — rather than deleting any
/// directory of that name at any depth, which is the kind of rule that
/// eventually meets a crate whose fixtures are named the same.
fn remove_incremental(root: &Path) -> io::Result<()> {
    let mut removed = Vec::new();
    for profile in profile_dirs(root) {
        let candidate = profile.join("incremental");
        if candidate.is_dir() {
            fs::remove_dir_all(&candidate)?;
            removed.push(candidate);
        }
    }
    info!(count = removed.len(), "cleared incremental caches");
    Ok(())
}

/// `<root>/*` and `<root>/*/*`, directories only: profiles live at one of those
/// two depths depending on whether the build was cross-compiled.
fn profile_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(nested) = fs::read_dir(&path) {
            out.extend(nested.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
        out.push(path);
    }
    out
}

/// Empty `root`, keeping `root` itself — see [`Tier::Wholesale`].
fn empty_dir(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)?.flatten() {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// A size a human can compare to `du -sh` without arithmetic.
///
/// Decimal units, for the reason the budget's own conversion is decimal: it is
/// written in GB and the report has to be readable against it.
pub fn humanize_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    match bytes {
        b if b >= GB => format!("{:.1} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1} kB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// The cadence is claimed on the **attempt**, not on a successful reading.
    /// Claiming on success re-walks a directory that cannot be read on every
    /// tick of a loop that ticks once a second, with a `warn!` each time.
    #[tokio::test]
    async fn an_unreadable_directory_still_claims_the_cadence() {
        let dir = VerifyDir::new(PathBuf::from("/nonexistent/verify-target"), 20);
        assert!(dir.maintain(true).await.is_none());
        assert!(
            dir.usage().is_none(),
            "a failed walk leaves no reading — never a zero, which reads as an empty \
             directory"
        );
        assert!(
            !dir.measure_due(Utc::now()),
            "the attempt claimed the window even though it produced nothing"
        );
    }

    /// Cargo hardlinks `<profile>/<bin>` to `<profile>/deps/<bin>-<hash>`. Two
    /// names for one file is one file's worth of disk, and a report that
    /// disagreed with the `du -sh` that found the problem would not be trusted
    /// a second time.
    #[test]
    fn a_hardlink_is_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("debug/deps/tasks-abc123"), 1_000);
        fs::hard_link(
            root.join("debug/deps/tasks-abc123"),
            root.join("debug/tasks"),
        )
        .unwrap();
        write(&root.join("debug/deps/other.rlib"), 500);

        let (bytes, files) = measure(root).unwrap();
        assert_eq!(bytes, 1_500);
        assert_eq!(files, 2);
    }

    /// Tier 1 on its own, which is the case the measured host is in: 24.24 GB
    /// of a 51 GB directory was `incremental/`, and every byte of it was keyed
    /// to a worktree path no later checkout could reuse. Nothing else is
    /// touched — the warmth is the whole point of the directory.
    #[tokio::test]
    async fn the_incremental_caches_go_first_and_cost_no_warmth() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("debug/incremental/tasks-1a2b/x.bin"), 3_000);
        write(&root.join("debug/deps/libwarmth.rlib"), 100);
        write(
            &root.join("aarch64-apple-darwin/debug/incremental/y.bin"),
            100,
        );

        let dir = VerifyDir::with_budget(root.clone(), Some(2_000));
        let Some(Notice::Reclaimed(reclaim)) = dir.maintain(true).await else {
            panic!("3 GB against a 2 GB ceiling is over it");
        };
        assert_eq!(reclaim.tier, Tier::Incremental);
        assert_eq!(reclaim.after, 100, "and it is measured, not estimated");
        assert!(
            root.join("debug/deps/libwarmth.rlib").exists(),
            "the warmth survives"
        );
        assert!(!root.join("debug/incremental").exists());
        assert!(
            !root.join("aarch64-apple-darwin/debug/incremental").exists(),
            "a cross-compiled profile sits one level deeper and counts too"
        );
        assert!(reclaim.describe(&root).contains("cost no warmth"));
    }

    /// Tier 2 when tier 1 was not enough — and it keeps the directory itself,
    /// which is created once per boot precisely so the prompt cannot name one
    /// the agent will find missing. It also has to say what it cost: a cold
    /// verification is what routes the next batch to a human.
    #[tokio::test]
    async fn the_wholesale_tier_empties_the_directory_and_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("debug/deps/huge.rlib"), 3_000);
        write(&root.join("CACHEDIR.TAG"), 10);

        let dir = VerifyDir::with_budget(root.clone(), Some(2_000));
        let Some(Notice::Reclaimed(reclaim)) = dir.maintain(true).await else {
            panic!("over the ceiling with nothing incremental to drop");
        };
        assert_eq!(reclaim.tier, Tier::Wholesale);
        assert_eq!(reclaim.after, 0);
        assert!(root.is_dir(), "the directory itself stays");
        assert!(!root.join("debug").exists());
        let said = reclaim.describe(&root);
        assert!(said.contains("THE NEXT VERIFICATION IS COLD"), "{said}");
        assert!(said.contains("goes to a human"), "{said}");

        // And it is still on `/status` afterwards, for the rest of the boot.
        let usage = dir.usage().expect("a reading");
        assert_eq!(usage.last_reclaim.unwrap().after_bytes, 0);
        assert!(!usage.over_budget);
    }

    /// `ORCHESTRATOR_TARGET_BUDGET_GB=0` — the `TASKS_UPDATE_HOLD=off` shape.
    /// The reclaim goes; the **report does not**, and is not switchable, because
    /// a directory that grows silently is exactly what #1010 was.
    #[tokio::test]
    async fn a_budget_of_zero_reports_and_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("debug/deps/huge.rlib"), 5_000);

        let dir = VerifyDir::new(root.clone(), 0);
        assert_eq!(dir.maintain(true).await, None);
        let usage = dir.usage().expect("measured anyway");
        assert_eq!(usage.bytes, 5_000);
        assert_eq!(usage.budget_bytes, None);
        assert!(!usage.over_budget, "there is no ceiling to be over");
        assert!(root.join("debug/deps/huge.rlib").exists());
    }

    /// A human with the orchestrator session checked out may be building in
    /// here, so that pass measures and reports and touches nothing. Announced
    /// once per edge: a complaint every fifteen minutes is one a reader learns
    /// to skip.
    #[tokio::test]
    async fn a_checked_out_session_is_reported_once_and_never_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write(&root.join("debug/deps/huge.rlib"), 3_000);

        let dir = VerifyDir::with_budget(root.clone(), Some(2_000));
        let Some(Notice::OverBudget { reading, budget }) = dir.maintain(false).await else {
            panic!("over the ceiling with nothing permitted to act");
        };
        assert_eq!(reading.bytes, 3_000);
        assert_eq!(budget, 2_000);
        assert!(root.join("debug/deps/huge.rlib").exists());
        assert!(dir.usage().unwrap().over_budget);

        // The cadence has to lapse before a second pass measures at all, and
        // when it does the edge has already been announced.
        dir.inner.lock().unwrap().measured_at = None;
        assert_eq!(dir.maintain(false).await, None, "announced once per edge");
    }

    /// The walk is hundreds of thousands of files, so it happens on a cadence
    /// and not at read time. Signed, like the two health records: a clock that
    /// stepped backwards walks rather than waiting out a window that will not
    /// end.
    #[test]
    fn the_walk_is_claimed_once_per_interval() {
        let dir = VerifyDir::new(PathBuf::from("/tmp"), 20);
        assert!(dir.measure_due(at(0)));
        assert!(!dir.measure_due(at(0)));
        assert!(!dir.measure_due(at(MEASURE_INTERVAL.as_secs() as i64 - 1)));
        assert!(dir.measure_due(at(MEASURE_INTERVAL.as_secs() as i64)));
        assert!(
            !dir.measure_due(at(-10_000)),
            "a clock that stepped backwards waits out the difference rather than \
             re-walking on every tick — see `measure_due`"
        );
    }

    /// Decimal units, because the budget is written in GB and the number has to
    /// read against the `du -sh` somebody ran. The app carries its own copy of
    /// this (it depends on `tasks-client`, not on `tasks`) with the same cases.
    #[test]
    fn sizes_read_the_way_du_prints_them() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(999), "999 B");
        assert_eq!(humanize_bytes(1_500), "1.5 kB");
        assert_eq!(humanize_bytes(2_500_000), "2.5 MB");
        assert_eq!(humanize_bytes(51_000_000_000), "51.0 GB");
    }
}
