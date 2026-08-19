//! The disclaimer is prose, and prose is the one deliverable nothing else in
//! this tree can keep honest (#984).
//!
//! Two surfaces say what running Tasks does to your machine and your GitHub
//! account: the README's `## Read this first`, which is canonical, and
//! `app-gpui/src/disclaimer.rs`, which is the same claims shorter for the
//! About window, the Server window's pipeline control and the Play tooltip.
//! They are two independent bodies of prose, and the drift that will actually
//! happen is one of them being corrected and the other not — the app being
//! the half a stranger reads before pointing this at their repositories. So
//! every act below is asserted in **both**, from one list.
//!
//! These tests live here rather than in `app-gpui` because `app-gpui` is not
//! a workspace member: `make test` never runs its tests, and a guard nothing
//! runs is not a guard. `app-gpui/src/disclaimer.rs` carries its own unit
//! tests as well; `make app-test` is what runs those.
//!
//! Both files are read from [`env!("CARGO_MANIFEST_DIR")`] rather than from a
//! relative path, so the tests do not depend on which directory the runner
//! happened to start in.
//!
//! What is asserted is **acts, never wording**. The copy stays free to be
//! rewritten; it only stops being free to stop saying what it says.

use std::fs;
use std::path::PathBuf;

/// The acts the copy exists to name, each checkable against the tree, and
/// each required in the README section *and* in the app's constants.
///
/// The right-hand side is a substring of the collapsed, lowercased prose.
/// They are short and verb-shaped for a reason: a longer one pins a sentence
/// rather than a claim, and a guard that goes red on a rewrite is a guard
/// people delete.
const ACTS: &[(&str, &str)] = &[
    // `images/{scout,builder}/Dockerfile` — the agent commands.
    (
        "agents run with permission checks off",
        "--dangerously-skip-permissions",
    ),
    // `Scopes::AGENT` in `crates/tasks/src/broker.rs`: `anthropic` +
    // `git-read`, repo-bound. The issue's own bullet list said agents hold a
    // token that can push; they do not, and the correction is the half most
    // likely to get flattened back into "agents can push".
    ("agent leases cannot push", "cannot push"),
    ("agent leases are repo-bound", "one repository"),
    // The server's own acts, under its own credential, on an agent's say-so.
    ("the server pushes", "pushes branches"),
    ("the server opens pull requests", "opens pull requests"),
    ("the server merges", "merges them"),
    ("the server comments upstream", "comments on issues"),
    ("the server closes issues", "closes them"),
    // `crates/tasks/src/server.rs`: no auth. `loopback.rs` refuses browser
    // requests, which is about pages and not about processes.
    ("the local api is unauthenticated", "no authentication"),
    // `ORCHESTRATOR_WORKDIR` / `ORCHESTRATOR_CMD`, `crates/tasks/src/run.rs`.
    ("the orchestrator runs outside a vm", "not in a vm"),
];

/// Phrases that mean the copy has retreated into boilerplate. Named in full
/// rather than left to "and friends", so the list is reviewable.
///
/// **This test carries no weight on its own.** A denylist is a proxy: prose
/// can be hedged into uselessness without using any of these words, and this
/// test would pass on it. [`the_disclaimer_names_what_the_system_actually_does`]
/// is the one doing the work — it fails when an act is softened into a
/// generality, which is the failure #984 was filed about. Do not read a green
/// suite as evidence the copy is still blunt.
const HEDGES: &[&str] = &[
    "at your own risk",
    "as-is basis",
    "no responsibility",
    "no liability",
    "not liable",
    "disclaim",
    "merchantability",
    "to the fullest extent",
    "under no circumstances",
    "makes no representation",
    "make no representation",
];

fn repo_root() -> PathBuf {
    // `crates/tasks` -> the repository root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

/// Collapsed, lowercased prose.
///
/// The README is hard-wrapped and the app's constants are wrapped with Rust
/// line continuations, so a raw `contains` would fail on a reflow rather than
/// on a rewrite — and a guard that goes red when someone runs a formatter is
/// a guard people delete. HTML comments are dropped rather than collapsed:
/// one of them would otherwise talk *about* the copy convincingly enough to
/// satisfy a check that the copy itself still says something.
fn prose(source: &str) -> String {
    let mut out = String::new();
    let mut rest = source;
    // HTML comments, which may span lines.
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => &rest[start + end + 3..],
            None => "",
        };
    }
    out.push_str(rest);

    let stripped = out;

    // Rust's `\`-at-end-of-line continuation eats the newline and the next
    // line's indentation; do the same, or a phrase split across two source
    // lines reads as containing a backslash.
    let mut joined = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'\n') {
            chars.next();
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                chars.next();
            }
            continue;
        }
        joined.push(ch);
    }

    joined
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The README's `## Read this first`, up to the next heading.
fn readme_section() -> String {
    let readme = read("README.md");
    let start = readme
        .find("## Read this first")
        .expect("README.md has a `## Read this first` section");
    let rest = &readme[start..];
    let end = rest[1..]
        .find("\n## ")
        .map(|at| at + 1)
        .unwrap_or(rest.len());
    prose(&rest[..end])
}

/// The app's copy: the value of every `pub const` in the module, and nothing
/// else in the file.
///
/// Reading the whole file instead is the mistake this function exists to not
/// make. The module's doc comment explains the claims and its unit tests
/// assert some of the same phrases, so a whole-file `contains` passes on a
/// module whose *constants* have been softened — which is this test's own
/// failure mode, one level up from the HTML comment [`prose`] strips.
fn app_copy() -> String {
    let source = read("app-gpui/src/disclaimer.rs");
    let mut copy = String::new();
    let mut found = 0;
    for declaration in source.split("pub const ").skip(1) {
        let (_, value) = declaration
            .split_once('=')
            .expect("a `pub const` has a value");
        let end = value
            .find(';')
            .expect("a `pub const` value ends in a semicolon");
        let value = &value[..end];
        let (open, close) = (value.find('"'), value.rfind('"'));
        if let (Some(open), Some(close)) = (open, close)
            && close > open
        {
            copy.push_str(&value[open + 1..close]);
            copy.push(' ');
            found += 1;
        }
    }
    assert!(
        found >= 5,
        "expected the five surfaces' constants in app-gpui/src/disclaimer.rs, found {found} —          if the module stopped being a flat list of string constants, this reader needs updating"
    );
    prose(&copy)
}

/// #984 asks for this above the architecture, not in a footer — a warning
/// below the thing it warns about is one you read after deciding.
#[test]
fn the_disclaimer_sits_above_the_architecture() {
    let readme = read("README.md");
    let first = readme
        .find("## Read this first")
        .expect("README.md has a `## Read this first` section");
    let idea = readme
        .find("## The idea")
        .expect("README.md still opens with `## The idea`");
    assert!(
        first < idea,
        "`## Read this first` must come before `## The idea`"
    );
}

/// The one carrying the weight: every act named, in both surfaces.
#[test]
fn the_disclaimer_names_what_the_system_actually_does() {
    let readme = readme_section();
    let app = app_copy();
    for (act, phrase) in ACTS {
        assert!(
            readme.contains(phrase),
            "README ▸ Read this first no longer names that {act} (looked for {phrase:?})"
        );
        assert!(
            app.contains(phrase),
            "app-gpui/src/disclaimer.rs no longer names that {act} (looked for {phrase:?}) — \
             the README still does, so the two surfaces have drifted"
        );
    }
}

/// A proxy, and only a proxy — see [`HEDGES`].
#[test]
fn the_disclaimer_does_not_hedge() {
    for (label, body) in [
        ("README ▸ Read this first", readme_section()),
        ("app-gpui/src/disclaimer.rs", app_copy()),
    ] {
        for hedge in HEDGES {
            assert!(
                !body.contains(hedge),
                "{label} has picked up {hedge:?}; the plain-English sentence is the point, \
                 and the legal register belongs in LICENSE"
            );
        }
    }
}
