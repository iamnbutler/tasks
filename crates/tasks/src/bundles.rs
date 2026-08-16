//! Bundles from builds whose branch could not be pushed.
//!
//! [`crate::builder::Builder`] tears the VM down *before* egress runs, so when
//! a push or a PR is refused the base64 bundle the VM sent is the only copy of
//! that implementation anywhere. It is written to
//! `<scratch_root>/rejected/<build_id>.bundle` — beside the per-build scratch
//! repo and deliberately outside it, because that repo is swept after every
//! build and this must not be.
//!
//! **The filesystem is the only record.** There is no table, no migration and
//! no cached size: `list` and `stat` are `read_dir` and `metadata`. A row
//! asserting that a bundle exists goes stale the moment somebody `rm`s one,
//! and this is explicitly a directory a human works in — recovering a bundle
//! means going to the server host and running the `git fetch`
//! [`recovery_command`] prints. Reading a directory that is normally absent
//! costs less than remembering what is in it.
//!
//! Two consequences of that, both deliberate:
//!
//! - [`RejectedBundles::list`] **ignores anything that is not
//!   `<build_id>.bundle`**. A `.bundle.bak` a human made halfway through a
//!   recovery must not turn the listing into an error, and a directory that
//!   does not exist is an empty list rather than a failure — the ordinary
//!   state of a server that has never had an egress fail.
//! - A bundle whose build row is gone (a wiped database, a hand-copied file)
//!   has no branch and no base, so nothing here can say how to recover it.
//!   The API skips those rather than inventing a command; the file stays on
//!   disk, which is the safe direction.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::warn;

use crate::models::BuildId;

/// Directory name under [`crate::builder::BuilderConfig::scratch_root`].
pub const REJECTED_DIR: &str = "rejected";

/// Suffix every bundle file carries. Anything else in the directory is a
/// human's business and is left alone.
const BUNDLE_SUFFIX: &str = ".bundle";

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("rejected bundle io: {0}")]
    Io(#[from] std::io::Error),
}

/// One bundle on disk, as the filesystem describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    pub build_id: BuildId,
    pub path: PathBuf,
    pub bytes: u64,
    /// The file's mtime — when egress failed and this was written.
    pub created_at: DateTime<Utc>,
}

/// The `rejected/` directory, addressed.
#[derive(Debug, Clone)]
pub struct RejectedBundles {
    dir: PathBuf,
}

impl RejectedBundles {
    /// Address `<scratch_root>/rejected/`. Creates nothing — the directory is
    /// made on the first [`Self::preserve`], and its absence is an empty list.
    pub fn under(scratch_root: impl AsRef<Path>) -> Self {
        Self {
            dir: scratch_root.as_ref().join(REJECTED_DIR),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a given build's bundle lives, whether or not it is there.
    pub fn path_for(&self, id: &BuildId) -> PathBuf {
        self.dir.join(format!("{id}{BUNDLE_SUFFIX}"))
    }

    /// Write a build's commits down. Returns the path they landed at.
    pub async fn preserve(&self, id: &BuildId, bytes: &[u8]) -> Result<PathBuf, BundleError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.path_for(id);
        tokio::fs::write(&path, bytes).await?;
        Ok(path)
    }

    /// One bundle, or `None` when there is none — the ordinary answer.
    pub async fn stat(&self, id: &BuildId) -> Result<Option<BundleFile>, BundleError> {
        let path = self.path_for(id);
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => Ok(Some(BundleFile {
                build_id: id.clone(),
                bytes: meta.len(),
                created_at: modified_at(&meta),
                path,
            })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Everything preserved, newest first.
    ///
    /// An absent directory is an empty list, not an error: a server that has
    /// never had an egress fail has never created it.
    pub async fn list(&self) -> Result<Vec<BundleFile>, BundleError> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut found = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_suffix(BUNDLE_SUFFIX) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let meta = match entry.metadata().await {
                Ok(meta) if meta.is_file() => meta,
                Ok(_) => continue,
                // A file that vanished between the listing and the stat: a
                // human deleting one, or a reclaim in the same breath. Not a
                // reason to fail the whole listing.
                Err(e) => {
                    warn!(path = %entry.path().display(), error = %e, "could not stat a bundle");
                    continue;
                }
            };
            found.push(BundleFile {
                build_id: BuildId::from_raw(id),
                path: entry.path(),
                bytes: meta.len(),
                created_at: modified_at(&meta),
            });
        }
        found.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.path.cmp(&b.path)));
        Ok(found)
    }

    /// Delete one. `false` when there was nothing there.
    pub async fn remove(&self, id: &BuildId) -> Result<bool, BundleError> {
        match tokio::fs::remove_file(self.path_for(id)).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The `git fetch` that reconstructs `branch` from the bundle at `path`.
///
/// Run in a repository that already has the build's `base_sha` — the bundle is
/// thin and carries the commits but not what they grew from. The path is
/// shell-quoted because it is a path on somebody's server and a human is going
/// to paste this into a shell.
pub fn recovery_command(path: &Path, branch: &str) -> String {
    format!(
        "git fetch {} '{branch}:{branch}'",
        shell_quote(&path.display().to_string())
    )
}

/// Single-quote for `sh`, the only quoting that needs no escape table: inside
/// `'…'` every byte is literal, and a literal `'` is spelled `'\''`.
fn shell_quote(raw: &str) -> String {
    if !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        return raw.to_string();
    }
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// The mtime, or now if the platform will not say. Only ever used to age and
/// order a listing, so a fallback is better than refusing to report the file.
fn modified_at(meta: &std::fs::Metadata) -> DateTime<Utc> {
    meta.modified()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> BuildId {
        BuildId::from_raw(raw)
    }

    #[tokio::test]
    async fn a_directory_that_was_never_created_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = RejectedBundles::under(tmp.path().join("nothing-here"));
        assert!(bundles.list().await.unwrap().is_empty());
        assert!(bundles.stat(&id("build_1")).await.unwrap().is_none());
        assert!(!bundles.remove(&id("build_1")).await.unwrap());
    }

    #[tokio::test]
    async fn preserve_then_stat_then_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = RejectedBundles::under(tmp.path());
        let path = bundles.preserve(&id("build_1"), b"PACK...").await.unwrap();
        assert!(path.ends_with("build_1.bundle"));

        let stat = bundles.stat(&id("build_1")).await.unwrap().unwrap();
        assert_eq!(stat.bytes, 7);
        assert_eq!(stat.build_id, id("build_1"));

        assert!(bundles.remove(&id("build_1")).await.unwrap());
        assert!(bundles.stat(&id("build_1")).await.unwrap().is_none());
        // Idempotent: a second delete is not an error, which is what lets a
        // reclaim and a human's Delete race without either one failing.
        assert!(!bundles.remove(&id("build_1")).await.unwrap());
    }

    /// The directory is one a human works in. A backup copy made halfway
    /// through a recovery, a stray note — none of it may turn the listing into
    /// an error or invent a build id.
    #[tokio::test]
    async fn only_build_id_dot_bundle_is_a_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = RejectedBundles::under(tmp.path());
        bundles.preserve(&id("build_1"), b"one").await.unwrap();
        for stray in ["build_1.bundle.bak", "notes.txt", ".bundle", "subdir"] {
            let path = bundles.dir().join(stray);
            if stray == "subdir" {
                tokio::fs::create_dir(&path).await.unwrap();
            } else {
                tokio::fs::write(&path, b"x").await.unwrap();
            }
        }
        let listed = bundles.list().await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].build_id, id("build_1"));
    }

    #[tokio::test]
    async fn a_listing_is_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles = RejectedBundles::under(tmp.path());
        for name in ["build_old", "build_new"] {
            bundles.preserve(&id(name), b"x").await.unwrap();
            // Filesystems on this box carry sub-second mtimes, but not every
            // one does; a beat apart makes the order the file's, not the
            // tiebreak's.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let listed = bundles.list().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|b| b.build_id.to_string())
                .collect::<Vec<_>>(),
            vec!["build_new", "build_old"]
        );
    }

    /// A path with a space in it is the ordinary case on a Mac, and the
    /// command is meant to be pasted into a shell.
    #[test]
    fn a_recovery_command_survives_a_paste() {
        let quoted = recovery_command(
            Path::new("/Users/someone/Library/Application Support/tasks/b.bundle"),
            "build/build_1",
        );
        assert_eq!(
            quoted,
            "git fetch '/Users/someone/Library/Application Support/tasks/b.bundle' \
             'build/build_1:build/build_1'"
        );
        // An ordinary path is left alone — quoting everything makes the
        // common case read like an escape hatch.
        assert_eq!(
            recovery_command(Path::new("/var/tasks/b.bundle"), "build/x"),
            "git fetch /var/tasks/b.bundle 'build/x:build/x'"
        );
        // And the one character single-quoting cannot contain.
        assert!(shell_quote("it's").contains("'\\''"));
    }
}
