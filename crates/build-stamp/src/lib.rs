//! Stamps a build identity into a binary so "is what I'm running fresh?" has
//! an answer.
//!
//! One call from a `build.rs` — [`emit`] — and the crate gets two compile-time
//! environment variables: `{PREFIX}_VERSION` is `0.1.<commit count>` and
//! `{PREFIX}_COMMIT` the short SHA (`-dirty` when the tree had uncommitted
//! changes). Read them back with `env!`:
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     build_stamp::emit("TASKS_SERVER");
//! }
//!
//! // src/version.rs
//! pub const VERSION: &str = env!("TASKS_SERVER_VERSION");
//! pub const COMMIT: &str = env!("TASKS_SERVER_COMMIT");
//! ```
//!
//! Both values can be set in the *environment* to override the git probe,
//! which is how `make app` guarantees an installed bundle is stamped exactly
//! rather than stamped from whatever the tree looked like.
//!
//! With no git in reach (a source tarball, a build in a container without the
//! repo) this falls back to the crate version and `unknown` — itself the tell
//! that you're not looking at a `make`-installed artifact.
//!
//! This exists so the server, the client and the app's About window are one
//! implementation of the scheme rather than three copies that drift apart —
//! and comparing two of these numbers is only meaningful because they are
//! computed the same way.

use std::env;
use std::path::Path;
use std::process::Command;

/// Emit `{prefix}_VERSION` / `{prefix}_COMMIT` as `rustc-env`, plus the
/// `rerun-if-changed` lines that keep them fresh. Call from `build.rs`.
///
/// Callers that need to watch more paths (the `tasks` crate watches
/// `migrations`, which sqlx embeds at compile time) may emit their own
/// `rerun-if-changed` lines alongside this call — the sets add up.
///
/// # Freshness
///
/// The git probe re-runs when `.git/HEAD` or `.git/index` moves, so a commit,
/// a checkout or a `git add` all take effect. A bare working-tree edit touches
/// neither, so the `-dirty` suffix can lag one build behind; watching `src`
/// covers edits to the crate itself, and the environment overrides are what
/// make an installed artifact exact.
pub fn emit(prefix: &str) {
    let version_key = format!("{prefix}_VERSION");
    let commit_key = format!("{prefix}_COMMIT");

    // An explicit value beats the probe; declare them so a Makefile's values
    // take effect on the very next build rather than a build later.
    println!("cargo:rerun-if-env-changed={version_key}");
    println!("cargo:rerun-if-env-changed={commit_key}");
    // Emitting any rerun-if-changed replaces cargo's default "watch the whole
    // package", so name the sources back explicitly.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    watch_git();

    let version = env_override(&version_key)
        .or_else(|| git(&["rev-list", "--count", "HEAD"]).map(|count| format!("0.1.{count}")))
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));

    let commit = env_override(&commit_key)
        .or_else(|| {
            git(&["rev-parse", "--short", "HEAD"]).map(|sha| {
                if git_tree_dirty() {
                    format!("{sha}-dirty")
                } else {
                    sha
                }
            })
        })
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env={version_key}={version}");
    println!("cargo:rustc-env={commit_key}={commit}");
}

fn env_override(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

/// Re-probe git when the checkout moves.
fn watch_git() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    for name in ["HEAD", "index"] {
        let path = Path::new(&git_dir).join(name);
        // A rerun-if-changed path that does not exist reads to cargo as
        // "always changed", which would rebuild the crate every time.
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn git_tree_dirty() -> bool {
    Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .status()
        .is_ok_and(|status| !status.success())
}
