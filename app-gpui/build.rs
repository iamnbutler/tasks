//! Stamps a version identity into the binary so "is what I'm running fresh?"
//! has an answer in About Tasks.
//!
//! `TASKS_GPUI_VERSION` is `0.1.<commit count>` and `TASKS_GPUI_COMMIT` the
//! short SHA (`-dirty` when the tree had uncommitted changes) — the same two
//! values the Swift app carried in MARKETING_VERSION / CURRENT_PROJECT_VERSION.
//! Both can be set in the environment to override the git probe, which is how
//! `make app` guarantees an installed bundle is stamped exactly.
//!
//! With no git in reach (a source tarball, a build in a container without the
//! repo) this falls back to the crate version and `unknown` — itself the tell
//! that you're not looking at a `make app` install.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    // An explicit value beats the probe; declare them so the Makefile's
    // values take effect on the very next build rather than a build later.
    println!("cargo:rerun-if-env-changed=TASKS_GPUI_VERSION");
    println!("cargo:rerun-if-env-changed=TASKS_GPUI_COMMIT");
    // Emitting any rerun-if-changed replaces cargo's default "watch the whole
    // package", so name the sources back explicitly.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    watch_git();

    let version = env_override("TASKS_GPUI_VERSION")
        .or_else(|| git(&["rev-list", "--count", "HEAD"]).map(|count| format!("0.1.{count}")))
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));

    let commit = env_override("TASKS_GPUI_COMMIT")
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

    println!("cargo:rustc-env=TASKS_GPUI_VERSION={version}");
    println!("cargo:rustc-env=TASKS_GPUI_COMMIT={commit}");
}

fn env_override(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

/// Re-probe git when the checkout moves. A bare working-tree edit touches
/// neither of these, so the `-dirty` suffix can lag a build behind — watching
/// `src` covers edits to this crate, and `make app` passes both values
/// explicitly so an installed bundle is never stale.
fn watch_git() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    for name in ["HEAD", "index"] {
        let path = Path::new(&git_dir).join(name);
        // A rerun-if-changed path that does not exist reads to cargo as
        // "always changed", which would rebuild this crate every time.
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
