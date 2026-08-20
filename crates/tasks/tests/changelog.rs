//! `scripts/changelog.sh` is the only generator of a CHANGELOG section, and a
//! release runs it exactly once against a range nobody will ever re-run (#997).
//!
//! These tests live in the server's tree for the reason
//! `crates/tasks/tests/disclaimer.rs` states in its own header: the script is
//! not a Rust artifact, and a guard that only runs when somebody remembers to
//! run it is not a guard. `make test` runs this; nothing else would.
//!
//! Every fixture is a **synthetic** git repository built in a tempdir — real
//! `git`, real commits, real processes, per this repo's no-mocks convention.
//! Not a fixed historical range, which is what the design doc suggested: a
//! Scout VM clones `--depth 50`, so `git rev-list --count HEAD` reads a
//! fraction of the truth there, and a test pinned to real history would pass
//! in exactly the two places nobody is watching and fail in the one an agent
//! reads.
//!
//! The child's `PATH` is pinned to `/usr/bin:/bin`, so a developer's
//! authenticated `gh` cannot answer with some other repository's PR titles —
//! and so the offline behaviour the Builder VM gets is the behaviour under
//! test. The environment is scrubbed of `CHANGELOG_*` for the same reason.
//!
//! What is asserted is **shape**: which commits are selected, what a subject
//! is rewritten to, and where the compare link is. The wording of the prose in
//! `CHANGELOG.md` is not this file's business.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // `crates/tasks` -> the workspace root, so the test does not depend on
    // which directory the runner happened to start in.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/tasks has two ancestors")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/changelog.sh")
}

/// A git command in `dir`, with an identity and a fixed branch name so the
/// test does not depend on the machine's git config.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A commit with `message` as its full message (subject plus optional body),
/// touching a uniquely-named file so nothing is ever empty.
fn commit(dir: &Path, name: &str, message: &str) {
    std::fs::write(dir.join(name), name).expect("write fixture file");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", message]);
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    dir
}

/// Run the script, with the environment the Builder VM has: no `CHANGELOG_*`
/// leaking in from the shell, and a `PATH` that cannot reach a developer's
/// authenticated `gh`.
fn run(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("bash");
    cmd.arg(script())
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.display().to_string());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("changelog.sh {args:?}: {e}"));
    assert!(
        out.status.success(),
        "changelog.sh {args:?} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The section's bullets, in order.
fn bullets(section: &str) -> Vec<String> {
    section
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(str::to_string)
        .collect()
}

const DETERMINISTIC: &[(&str, &str)] = &[
    ("CHANGELOG_VERSION", "0.1.7"),
    ("CHANGELOG_DATE", "2026-08-20"),
];

#[test]
fn next_version_is_one_past_the_commit_count() {
    let dir = init_repo();
    commit(dir.path(), "a", "first");
    commit(dir.path(), "b", "second");

    // Two commits, so the next release — whose own changelog commit is inside
    // it — is 0.1.3. This is the only place that arithmetic is written.
    assert_eq!(run(dir.path(), &["--next-version"], &[]).trim(), "0.1.3");
}

#[test]
fn next_version_degrades_to_nothing_outside_a_repository() {
    // Expanded on every `make` invocation, including `make test-ci` inside a
    // Builder VM. A failure here has to be empty output and a zero exit, not a
    // shell error that would break every target in the tree.
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(run(dir.path(), &["--next-version"], &[]).trim(), "");
}

#[test]
fn the_heading_carries_the_version_and_the_date() {
    let dir = init_repo();
    commit(dir.path(), "a", "A change that reads as a sentence (#1)");

    let section = run(dir.path(), &["", "HEAD"], DETERMINISTIC);
    assert!(
        section.starts_with("## v0.1.7 — 2026-08-20\n"),
        "unexpected heading: {section}"
    );
}

#[test]
fn an_ordinary_subject_survives_verbatim() {
    let dir = init_repo();
    commit(
        dir.path(),
        "a",
        "A broker outage holds dispatch instead of destroying the queue (#1006)",
    );

    assert_eq!(
        bullets(&run(dir.path(), &["", "HEAD"], DETERMINISTIC)),
        vec!["A broker outage holds dispatch instead of destroying the queue (#1006)"]
    );
}

#[test]
fn a_github_merge_takes_its_title_from_the_commit_body() {
    let dir = init_repo();
    let path = dir.path();
    commit(path, "base", "base");
    git(path, &["checkout", "-q", "-b", "feature"]);
    commit(path, "f", "work on the branch");
    git(path, &["checkout", "-q", "main"]);
    git(
        path,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge pull request #758 from iamnbutler/feat/mac-app\n\n\
             SwiftUI mac app: read-only dashboard over the Tasks API",
            "feature",
        ],
    );

    // GitHub already put the PR title in the body, so the `gh` call is the
    // fallback rather than the rule — free, offline and un-rate-limitable.
    // `PATH` here cannot reach an authenticated `gh` at all, which is what
    // makes this assertion about the body and nothing else.
    let listed = bullets(&run(path, &["", "HEAD"], DETERMINISTIC));
    assert!(
        listed.contains(&"SwiftUI mac app: read-only dashboard over the Tasks API".to_string()),
        "body title not used: {listed:?}"
    );
    assert!(
        !listed.iter().any(|l| l.starts_with("Merge pull request")),
        "raw merge subject leaked: {listed:?}"
    );
}

#[test]
fn a_pipeline_merge_subject_is_stripped_to_its_title() {
    let dir = init_repo();
    let path = dir.path();
    commit(path, "base", "base");
    git(path, &["checkout", "-q", "-b", "build"]);
    commit(path, "f", "implementation");
    git(path, &["checkout", "-q", "main"]);
    git(
        path,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge PR #1044: Squash strands every build stacked on the branch",
            "build",
        ],
    );

    let listed = bullets(&run(path, &["", "HEAD"], DETERMINISTIC));
    assert!(
        listed.contains(&"Squash strands every build stacked on the branch".to_string()),
        "pipeline merge subject not stripped: {listed:?}"
    );
}

#[test]
fn housekeeping_is_dropped_by_the_stated_denylist() {
    let dir = init_repo();
    let path = dir.path();
    commit(path, "a", "A real change (#1)");
    commit(path, "b", "Sweep: work the agent left uncommitted");
    git(path, &["checkout", "-q", "-b", "side"]);
    commit(path, "c", "side work");
    git(path, &["checkout", "-q", "main"]);
    git(
        path,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge branch 'side' into main",
            "side",
        ],
    );

    let listed = bullets(&run(path, &["", "HEAD"], DETERMINISTIC));
    assert_eq!(
        listed,
        vec!["A real change (#1)"],
        "denylist let noise through"
    );
}

#[test]
fn a_pull_request_merged_into_another_branch_is_not_lost() {
    // The correctness bug `--first-parent` alone has, demonstrated on this
    // repository's own history (28c879e, "Merge pull request #758"): a build
    // merged into another build's branch rather than into the trunk is
    // reachable from main and off main's first-parent chain, so a
    // first-parent-only walk drops it silently. This pipeline stacks builds
    // routinely, so it is structural rather than a one-off.
    let dir = init_repo();
    let path = dir.path();
    commit(path, "base", "base");

    git(path, &["checkout", "-q", "-b", "build-a"]);
    commit(path, "a", "work in build A");

    git(path, &["checkout", "-q", "-b", "build-b"]);
    commit(path, "b", "work in build B");

    // B lands on A's branch, not on the trunk.
    git(path, &["checkout", "-q", "build-a"]);
    git(
        path,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge pull request #2 from iamnbutler/build-b\n\nThe stacked change",
            "build-b",
        ],
    );

    // ...and only then does A land on the trunk.
    git(path, &["checkout", "-q", "main"]);
    git(
        path,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge pull request #1 from iamnbutler/build-a\n\nThe base change",
            "build-a",
        ],
    );

    let listed = bullets(&run(path, &["", "HEAD"], DETERMINISTIC));
    assert!(
        listed.contains(&"The base change".to_string()),
        "trunk landing missing: {listed:?}"
    );
    assert!(
        listed.contains(&"The stacked change".to_string()),
        "a pull request that landed on another branch was dropped: {listed:?}"
    );
    // The ordinary second parents of those merges are neither on the trunk nor
    // pull-request merges, so they stay out.
    assert!(
        !listed.iter().any(|l| l == "work in build B"),
        "an ordinary side commit was picked up: {listed:?}"
    );
}

#[test]
fn the_bootstrap_section_omits_the_compare_link() {
    let dir = init_repo();
    commit(dir.path(), "a", "A change (#1)");

    // There is no earlier tag to compare against, and the link would 404.
    let section = run(dir.path(), &["", "HEAD"], DETERMINISTIC);
    assert!(
        !section.contains("[full diff]"),
        "bootstrap section carries a compare link: {section}"
    );
}

#[test]
fn a_range_section_carries_the_compare_link_and_only_the_range() {
    let dir = init_repo();
    let path = dir.path();
    commit(path, "a", "Before the tag (#1)");
    git(path, &["tag", "-a", "v0.1.6", "-m", "v0.1.6"]);
    commit(path, "b", "After the tag (#2)");

    let section = run(path, &["v0.1.6", "HEAD"], DETERMINISTIC);
    assert_eq!(bullets(&section), vec!["After the tag (#2)"]);
    assert!(
        section
            .contains("[full diff](https://github.com/iamnbutler/tasks/compare/v0.1.6...v0.1.7)"),
        "missing or wrong compare link: {section}"
    );
}

#[test]
fn a_headline_lands_above_the_bullets() {
    let dir = init_repo();
    commit(dir.path(), "a", "A change (#1)");

    let mut env = DETERMINISTIC.to_vec();
    env.push(("CHANGELOG_HEADLINE", "The first release."));
    let section = run(dir.path(), &["", "HEAD"], &env);

    let headline = section
        .find("The first release.")
        .expect("headline present");
    let bullet = section.find("- A change (#1)").expect("bullet present");
    assert!(
        headline < bullet,
        "headline is below the bullets: {section}"
    );
}
