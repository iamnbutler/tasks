//! Guards for `CLAUDE.md` — the file every agent turn loads before it does
//! anything else.
//!
//! It is loaded in full, on every turn, by every agent working in this
//! repository, and it is the one file here whose cost is paid per-turn rather
//! than per-read. That makes its size a repository-level fact worth pinning,
//! and `crates/tasks/tests/site.rs` is the precedent for pinning one.
//!
//! The failure this exists to catch already happened once, and it was silent
//! (#1093). Between 2026-08-14 and 2026-08-21 the file grew from 8,823
//! characters to **190,413** — 21× in seven days, across 120 commits — because
//! each build recorded its reasoning and its rejected alternatives here. That
//! is good discipline aimed at the wrong file: a decision log grows without
//! bound by definition, an instruction file has a ceiling, and nothing noticed
//! the collision because no build reads the whole file and no test asserted its
//! size. By the end the section of universal rules alone was over the 130,000
//! character context budget, so the project structure, the test conventions and
//! everything an agent needs to *operate* had been pushed past the line by
//! prose about *why*.
//!
//! Two assertions, protecting two different things.
//!
//! [`claude_md_stays_within_its_budget`] is the size gate. Its failure message
//! carries the *rule* and not just the number, deliberately: a limit stated as
//! a bare integer teaches the next person to raise the integer, and raising it
//! is exactly the move that reproduces #1093 one notch higher.
//!
//! [`claude_md_paths_all_resolve`] is the other half, and it is what makes the
//! first one survivable. Cutting the file to a budget only works if the
//! reasoning has somewhere to be, so `CLAUDE.md` now indexes the module doc
//! comments and design documents that hold it. An index is only worth having
//! while it is true — a stale pointer sends a reader to a file that no longer
//! exists and teaches them the index cannot be trusted — so every path the file
//! names has to resolve.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The ceiling, in characters.
///
/// About 10% of what #1093 found, and roughly double what the split actually
/// landed at, so a genuinely universal addition never has to fight this test to
/// get in. It is comfortably inside the 130,000 the whole file has to share
/// with everything else in a turn.
///
/// **Raising this is almost always the wrong fix.** If a change needs more room
/// here, the thing that grew is nearly always reasoning about one module, and
/// reasoning about one module belongs in that module's `//!` header, where it
/// is read by whoever edits the code and deleted when the code is.
const BUDGET: usize = 20_000;

/// Paths that appear in `CLAUDE.md` as illustrations rather than as pointers,
/// and so are not expected to exist.
///
/// Listed rather than pattern-matched: an illustrative path is a judgement, and
/// a pattern that skipped "anything under `migrations/`" would also skip a real
/// pointer that had gone stale.
const ILLUSTRATIVE: &[&str] = &[
    // The shape a `make migration` filename takes, not a migration we have.
    "crates/tasks/migrations/20260815030411_build_transcripts.sql",
    // The two files a Scout writes, inside its VM.
    "SPEC.md",
    "NOTES.md",
    // `build.rs`, discussed as a kind of file rather than as one path.
    "build.rs",
];

fn repo_root() -> PathBuf {
    // `crates/tasks` -> the repository root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn claude_md() -> String {
    let path = repo_root().join("CLAUDE.md");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

#[test]
fn claude_md_stays_within_its_budget() {
    let source = claude_md();
    let size = source.chars().count();

    assert!(
        size <= BUDGET,
        "CLAUDE.md is {size} characters, over its {BUDGET} budget.\n\
         \n\
         CLAUDE.md holds the rules that hold everywhere in this repo, and \
         nothing else. It is not a decision log: it is loaded in full into \
         every agent turn, so anything in it that only matters while editing \
         one module is a cost paid by every turn that never reaches it.\n\
         \n\
         Before raising the budget, check whether what grew is reasoning about \
         one module. If it is, it belongs in that module's `//!` header, next \
         to the code it describes — where whoever changes the behaviour is \
         looking at the paragraph that explains it, and where deleting the code \
         deletes the explanation. A design that spans modules goes in \
         `docs/plans/`; how to run a server goes in `docs/operating.md`.\n\
         \n\
         This file was 190,413 characters in August 2026 (#1093) because that \
         check was never made, one paragraph at a time."
    );
}

/// Every repository path `CLAUDE.md` names must resolve.
///
/// Only paths that are unambiguously paths are checked: a backticked span
/// containing a `/` and no space, or one ending in a source-file extension.
/// Globs are skipped — `crates/vm-pool/*` is a statement about a directory's
/// contents rather than a pointer at a file.
#[test]
fn claude_md_paths_all_resolve() {
    let source = claude_md();
    let root = repo_root();

    let mut checked = 0usize;
    let mut missing: BTreeSet<String> = BTreeSet::new();

    for span in backticked(&source) {
        if !looks_like_a_path(span) || ILLUSTRATIVE.contains(&span) {
            continue;
        }
        checked += 1;
        if !root.join(span).exists() {
            missing.insert(span.to_string());
        }
    }

    assert!(
        checked > 10,
        "only {checked} paths were checked in CLAUDE.md — the extractor has \
         probably stopped recognising them, which would make this test pass \
         by finding nothing"
    );

    assert!(
        missing.is_empty(),
        "CLAUDE.md points at {} path(s) that do not exist:\n{}\n\n\
         CLAUDE.md indexes where the reasoning for this system lives — the \
         module doc comments and design documents that hold what used to be \
         inline. A pointer that no longer resolves is worse than no pointer: it \
         teaches the next reader that the index cannot be trusted, and the \
         index is the whole mechanism keeping this file small. Repoint it at \
         wherever the prose moved, or drop the line.\n\n\
         If the path is an illustration rather than a pointer, add it to \
         `ILLUSTRATIVE` in this file.",
        missing.len(),
        missing
            .iter()
            .map(|path| format!("  - {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The contents of every `` `backticked` `` span, in order.
fn backticked(source: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = source;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        match rest.find('`') {
            Some(close) => {
                spans.push(&rest[..close]);
                rest = &rest[close + 1..];
            }
            None => break,
        }
    }
    spans
}

/// Whether a backticked span is a pointer at something in this repository.
fn looks_like_a_path(span: &str) -> bool {
    if span.is_empty() || span.contains(char::is_whitespace) || span.contains('*') {
        return false;
    }
    // A directory or a nested file.
    let nested = span.contains('/');
    // A bare filename we still expect to find at the root.
    let sourceish = Path::new(span)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "rs" | "md" | "sql" | "toml" | "yml" | "sh"));
    (nested || sourceish)
        // `POST /tasks/{id}/queue` and friends are routes, not paths.
        && !span.starts_with('/')
        && !span.contains('{')
        // URLs.
        && !span.contains("://")
}
