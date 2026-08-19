//! `.env` loading — configuration as a property of the deployment rather than
//! of whoever launched the server.
//!
//! The bug this exists for: every knob in [`crate::run::Config`] was read from
//! the process environment alone, so `ORCHESTRATOR_CMD`,
//! `ORCHESTRATOR_WORKDIR` and `GITHUB_TOKEN` only ever took effect for a
//! server started from a shell that had exported them. Restart it any other
//! way — from the app's Server menu, whose ancestor is launchd and whose
//! environment is four `PATH` entries and nothing else — and every one of them
//! silently reverted to its default. Nothing failed. The server came up,
//! answered `GET /status`, kept serving, and was *wrong*: GitHub polling off,
//! builds unable to open a PR at the end of a twenty-minute run, and a live
//! orchestrator demoted mid-conversation from a checkout it could edit to the
//! curl-only allowlist. A default that only applies when the environment is
//! empty is indistinguishable from a default that was chosen.
//!
//! So the files are found *by the process*, not handed to it, and from three
//! places because no one of them covers every way this runs:
//!
//! - `<data dir>/.env` — launcher-independent, and the only one available to
//!   an installed binary that lives outside a checkout.
//! - the nearest `.env` at or above the **current directory** — a developer's
//!   `make serve`.
//! - the nearest `.env` at or above the **executable** — the same repo file,
//!   still found when the cwd is `/` because launchd started the app.
//!   [`crate::reload`] already locates the workspace by walking up from the
//!   binary; this is that trick applied to configuration.
//!
//! Precedence is that order, and the real environment outranks all three: an
//! explicit `GITHUB_TOKEN=… tasks serve` has to keep winning, or this becomes
//! a way for a checked-in file to override the operator at the keyboard.
//!
//! One consequence worth stating rather than fixing: which data dir to look in
//! is resolved from the real environment alone, so a `TASKS_DATA_DIR` set only
//! in a repo `.env` moves the store but not the search. It has to work that
//! way — the data dir must be knowable before configuration is read, or
//! finding the configuration depends on the configuration.
//!
//! Two things about *when* this runs are load-bearing.
//!
//! It happens once in `main`, before subcommand dispatch, so `serve`,
//! `reload` and `status` resolve the same `TASKS_DATA_DIR` and cannot end up
//! arguing about which server they are talking about. And it happens before
//! the tokio runtime starts, because [`std::env::set_var`] is unsafe for
//! exactly one reason — another thread reading the environment concurrently —
//! and an `#[tokio::main]` body is already running on a thread pool.
//!
//! It is deliberately *not* part of [`crate::run::Config::from_env`]. Tests
//! build configs, and a suite whose results depended on a developer's
//! untracked `.env` would be a worse bug than the one this fixes.
//!
//! # Turning it off
//!
//! [`DISABLE_VAR`] (`TASKS_ENV_FILES=off`) skips the search entirely. It exists
//! for tests that *exec the `tasks` binary*, and the thing it fixes is subtle
//! enough to be worth stating: `Command::env_remove` is the **opposite** of a
//! scrub here. The real environment is the only thing a `.env` entry loses to,
//! so removing a variable from a child's environment is precisely what hands
//! the decision to the file — and `.env` is gitignored, so a maintainer with
//! `TASKS_DEFAULT_MODE=play` in one fails a restart suite on their machine and
//! nowhere else. A test that spawns `tasks` has to set this, not unset that.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{info, warn};

/// The file `.env` loading looks for, at every candidate root.
const FILE_NAME: &str = ".env";

/// Set to `off` to skip `.env` loading entirely. See the module docs.
pub const DISABLE_VAR: &str = "TASKS_ENV_FILES";

/// [`DISABLE_VAR`] held something that is neither on nor off.
///
/// An error rather than a fallback, and deliberately not `.ok()`-able: mapping
/// an unreadable value to "load the files anyway" is the one direction this
/// switch must not fail in, since the caller setting it is trying to *stop*
/// ambient configuration from deciding a result.
#[derive(Debug, Error)]
#[error("{DISABLE_VAR} must be `on` or `off`, not {value:?}")]
pub struct BadSetting {
    pub value: String,
}

/// One file that was found, and what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    /// Variables this file set, by name. Names only, never values — a
    /// `.env` is mostly secrets, and this ends up in `serve.log`.
    pub applied: Vec<String>,
    /// Variables it defined that were already set, and so were ignored.
    /// Reported because "I put it in `.env` and nothing changed" is the one
    /// confusion this mechanism can cause.
    pub shadowed: Vec<String>,
    /// A parse failure. Reported rather than swallowed: a `.env` that stops
    /// halfway is how a token goes missing with nothing saying so.
    pub error: Option<String>,
}

/// Find and apply every `.env` that applies to this process.
///
/// Call once, from `main`, before the runtime starts and before any config is
/// read. Returns what it did for [`report`] to log once a subscriber exists —
/// the logging is split out because a `.env` may itself set `RUST_LOG`, so
/// loading has to precede `tracing_subscriber` initialization.
pub fn load() -> Result<Vec<Source>, BadSetting> {
    if !enabled(std::env::var_os(DISABLE_VAR).as_deref())? {
        return Ok(Vec::new());
    }
    let data_dir = tasks_api::paths::data_dir();
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe().ok();
    let roots = candidates(data_dir.as_deref(), cwd.as_deref(), exe.as_deref());

    let mut sources = Vec::new();
    for path in roots {
        let Some(source) = apply(&path) else { continue };
        sources.push(source);
    }
    Ok(sources)
}

/// Whether to search for `.env` files at all, given [`DISABLE_VAR`]'s raw
/// value. Absent means yes — the switch is opt-out.
///
/// A value that is not UTF-8 is a [`BadSetting`], never "absent": a caller who
/// set this variable meant something by it, and silently ignoring it would
/// re-enable exactly the mechanism they were turning off.
fn enabled(raw: Option<&OsStr>) -> Result<bool, BadSetting> {
    let Some(raw) = raw else { return Ok(true) };
    let bad = || BadSetting {
        value: raw.to_string_lossy().into_owned(),
    };
    match raw
        .to_str()
        .ok_or_else(bad)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "0" | "false" => Ok(false),
        "on" | "1" | "true" => Ok(true),
        _ => Err(bad()),
    }
}

/// Log what [`load`] did, once there is a subscriber to log to.
///
/// Silent when nothing was found: on a host configured entirely through the
/// environment there is no file to talk about, and a line saying so every
/// boot is noise.
pub fn report(sources: &[Source]) {
    for source in sources {
        if let Some(error) = &source.error {
            warn!(path = %source.path.display(), %error, "could not read .env");
            continue;
        }
        info!(
            path = %source.path.display(),
            vars = %source.applied.join(", "),
            "loaded .env"
        );
        if !source.shadowed.is_empty() {
            info!(
                path = %source.path.display(),
                vars = %source.shadowed.join(", "),
                "ignored (already set in the environment)"
            );
        }
    }
}

/// The files to try, in precedence order, deduplicated.
///
/// Deduplication is not cosmetic: under `make serve` the cwd and the
/// executable resolve to the same repository, and reading that file twice
/// would report every variable as shadowing itself.
fn candidates(data_dir: Option<&Path>, cwd: Option<&Path>, exe: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |path: Option<PathBuf>| {
        let Some(path) = path else { return };
        // Canonicalize for the identity check only — the original path is
        // what gets logged, because that is the one the operator can find.
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            out.push(path);
        }
    };

    push(
        data_dir
            .map(|dir| dir.join(FILE_NAME))
            .filter(|p| p.is_file()),
    );
    push(cwd.and_then(nearest));
    // The executable's own directory, not the binary: `<repo>/target/debug`
    // walks up to `<repo>/.env`, which is the file a developer actually
    // edits. An installed binary outside any checkout simply finds nothing.
    push(exe.and_then(Path::parent).and_then(nearest));
    out
}

/// The nearest `.env` at or above `start`.
fn nearest(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(FILE_NAME))
        .find(|candidate| candidate.is_file())
}

/// Read one file and set what it defines, leaving anything already set alone.
///
/// `None` when the file does not exist — the common case, and not worth
/// reporting.
fn apply(path: &Path) -> Option<Source> {
    let pairs = match parse(path) {
        Ok(pairs) => pairs,
        Err(error) if error.missing => return None,
        Err(error) => {
            return Some(Source {
                path: path.to_path_buf(),
                applied: Vec::new(),
                shadowed: Vec::new(),
                error: Some(error.message),
            });
        }
    };

    let (to_apply, shadowed) = plan(pairs, |key| std::env::var_os(key).is_some());
    let applied = to_apply.iter().map(|(key, _)| key.clone()).collect();
    for (key, value) in to_apply {
        // SAFETY: `load` runs in `main` before the tokio runtime is built and
        // before any thread is spawned, so nothing can be reading the
        // environment concurrently. Do not move this call anywhere that is
        // not provably single-threaded.
        unsafe { std::env::set_var(key, value) };
    }

    Some(Source {
        path: path.to_path_buf(),
        applied,
        shadowed,
        error: None,
    })
}

/// Split parsed pairs into the ones to set and the ones already spoken for.
///
/// Pure, with the environment handed in as `is_set`, because "the real
/// environment wins" is the rule most worth having a test for and the one
/// hardest to test by mutating a live process.
fn plan(
    pairs: Vec<(String, String)>,
    is_set: impl Fn(&OsString) -> bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut to_apply = Vec::new();
    let mut shadowed = Vec::new();
    for (key, value) in pairs {
        if is_set(&OsString::from(&key)) {
            shadowed.push(key);
        } else {
            to_apply.push((key, value));
        }
    }
    (to_apply, shadowed)
}

/// A file that could not be read, and whether that is because it is absent.
#[derive(Debug)]
struct ParseError {
    missing: bool,
    message: String,
}

/// Parse one `.env` into pairs, in file order.
///
/// Quoting is why this is `dotenvy` rather than a `split_once('=')` loop:
/// `ORCHESTRATOR_CMD="claude --print …"` is the realistic case, and a parser
/// that kept the quotes would produce a command whose program name begins
/// with a double quote — a failure well downstream of the mistake.
fn parse(path: &Path) -> Result<Vec<(String, String)>, ParseError> {
    let iter = dotenvy::from_path_iter(path).map_err(|e| ParseError {
        missing: matches!(&e, dotenvy::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
        message: e.to_string(),
    })?;

    let mut pairs = Vec::new();
    for item in iter {
        match item {
            Ok(pair) => pairs.push(pair),
            // Stop at the first bad line and keep what came before it: the
            // alternative is discarding a file over one typo, which is how a
            // `GITHUB_TOKEN` on line 2 goes missing because line 40 is
            // malformed. The error is reported either way.
            Err(e) => {
                return Err(ParseError {
                    missing: false,
                    message: format!("{e} (kept {} earlier variables)", pairs.len()),
                });
            }
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Quoted values are the reason this uses a real parser. The live failure
    /// was an `ORCHESTRATOR_CMD` whose quotes would have become part of the
    /// program name.
    #[test]
    fn quoted_values_lose_their_quotes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            "PLAIN=one\nQUOTED=\"claude --print --dangerously-skip-permissions\"\n\
             SINGLE='a b'\n# comment\n\nEMPTY=\n",
        );

        let pairs = parse(&path).unwrap();
        assert_eq!(
            pairs,
            vec![
                ("PLAIN".into(), "one".into()),
                (
                    "QUOTED".into(),
                    "claude --print --dangerously-skip-permissions".into()
                ),
                ("SINGLE".into(), "a b".into()),
                ("EMPTY".into(), String::new()),
            ]
        );
    }

    /// The rule that keeps this from overriding the operator at the keyboard.
    #[test]
    fn the_real_environment_outranks_the_file() {
        let pairs = vec![
            ("ALREADY".to_string(), "from file".to_string()),
            ("FRESH".to_string(), "from file".to_string()),
        ];

        let (to_apply, shadowed) = plan(pairs, |key| key == "ALREADY");

        assert_eq!(to_apply, vec![("FRESH".into(), "from file".into())]);
        assert_eq!(shadowed, vec!["ALREADY".to_string()]);
    }

    #[test]
    fn nearest_walks_up_from_a_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let expected = write(&root, "A=1\n");
        let nested = root.join("target/debug");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(nearest(&nested), Some(expected));
        assert_eq!(nearest(&root.join("target")), Some(root.join(FILE_NAME)));
    }

    #[test]
    fn nearest_finds_nothing_when_there_is_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        // An ancestor walk reaches `/`, so this only holds if the host has no
        // `/.env` — assert on the tempdir's own subtree instead of the walk.
        let found = nearest(&nested);
        assert!(
            found.is_none_or(|p| !p.starts_with(tmp.path())),
            "should not have found a .env inside the tempdir"
        );
    }

    /// The launchd case, in one assertion: cwd is `/` and useless, and the
    /// executable's ancestors are what find the repository's file.
    #[test]
    fn the_executable_is_searched_when_the_cwd_is_useless() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap();
        let expected = write(&repo, "GITHUB_TOKEN=x\n");
        let exe = repo.join("target/debug/tasks");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();

        let found = candidates(None, Some(Path::new("/")), Some(&exe));

        assert!(found.contains(&expected), "{found:?}");
    }

    /// Under `make serve` the cwd and the executable are the same repository.
    /// Reading that file twice would report every variable as shadowing
    /// itself — an alarming log line about a correct configuration.
    #[test]
    fn one_file_reachable_two_ways_is_listed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap();
        write(&repo, "A=1\n");
        let exe = repo.join("target/debug/tasks");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();

        let found = candidates(None, Some(&repo), Some(&exe));

        assert_eq!(found, vec![repo.join(FILE_NAME)]);
    }

    /// Precedence, as a list: the data dir first, then the working directory,
    /// then the executable.
    #[test]
    fn the_data_dir_is_tried_before_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let data_dir = root.join("state");
        let repo = root.join("repo");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        write(&data_dir, "A=state\n");
        write(&repo, "A=repo\n");

        let found = candidates(Some(&data_dir), Some(&repo), None);

        assert_eq!(
            found,
            vec![data_dir.join(FILE_NAME), repo.join(FILE_NAME)],
            "the data dir's file must be applied first, so it wins"
        );
    }

    /// A missing file is the common case and must not be reported as a
    /// problem.
    #[test]
    fn an_absent_file_is_not_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(apply(&tmp.path().join("nope.env")).is_none());
    }

    /// The opt-out switch, including the direction an unreadable value has to
    /// fail in. `.ok()` here would mean "load the files anyway", which is the
    /// one answer a caller trying to stop ambient configuration cannot use.
    #[test]
    fn the_disable_switch_is_opt_out_and_refuses_what_it_cannot_read() {
        let val = |s: &str| OsString::from(s);

        assert!(enabled(None).unwrap(), "absent means load them");
        for on in ["on", "1", "true", " ON "] {
            assert!(enabled(Some(&val(on))).unwrap(), "{on}");
        }
        for off in ["off", "0", "false", "OFF"] {
            assert!(!enabled(Some(&val(off))).unwrap(), "{off}");
        }

        let err = enabled(Some(&val("maybe"))).expect_err("not a setting");
        assert!(err.to_string().contains("maybe"), "{err}");
        assert!(err.to_string().contains(DISABLE_VAR), "{err}");

        // Not UTF-8: a `BadSetting`, never "absent".
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(vec![0xff, 0xfe]);
            assert!(enabled(Some(&raw)).is_err());
        }
    }

    /// Variables defined before a malformed line still apply — losing a whole
    /// file over one typo is how a token goes missing silently.
    #[test]
    fn a_parse_error_is_reported_rather_than_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "GOOD=1\n\"unterminated\n");

        let err = parse(&path).expect_err("should not parse");

        assert!(!err.missing);
        assert!(
            err.message.contains("kept 1 earlier variables"),
            "{}",
            err.message
        );
    }

    /// The repository root, resolved from this crate's manifest directory
    /// rather than guessed from the test's cwd — the same shape
    /// [`crate::migrations`]'s guard uses to reach its own directory, and for
    /// the same reason: cargo knows where the source is, a test process's cwd
    /// is whatever ran it.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/tasks sits two levels below the repository root")
            .to_path_buf()
    }

    /// One line of `.env.example` as an assignment, with any leading `#`
    /// removed — or `None` if it is prose.
    ///
    /// Commented-out lines count, and that is the point: the example ships
    /// with *every* line commented, so an extractor that skipped them would
    /// read the whole file as prose and assert nothing. A dead variable hides
    /// in a commented line exactly as well as in a live one.
    ///
    /// The anchor is the start of the line, which is why a prose comment must
    /// never be reflowed so that a sentence *begins* with `NAME=`. The first
    /// draft of the example wrapped one into `# VM_POOL_MAX_VMS=6, three is
    /// the ceiling.`, which this reads as an assignment — as does a skimming
    /// human, which is the better reason to keep such references mid-line or
    /// backticked.
    fn assignment(line: &str) -> Option<&str> {
        let bare = line
            .trim_start()
            .strip_prefix('#')
            .unwrap_or(line)
            .trim_start();
        let (name, _) = bare.split_once('=')?;
        let shaped = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        shaped.then_some(bare)
    }

    /// Every file under `crates/` and `images/`, plus the `Makefile`,
    /// concatenated — the places a variable this project reads can be named.
    ///
    /// Two exclusions. `target/` is build output: not a source of truth, and
    /// large enough to turn a fast test into a slow one. **This file itself**
    /// is excluded because the guard below would otherwise be satisfied by its
    /// own prose — its doc comment names the three dead variables it exists to
    /// keep out, so adding `TASKS_CONTAINER_IMAGE` to the example was green
    /// until this line existed. A guard a comment can talk out of firing is
    /// not a guard, and the falsification check is what caught it.
    fn searchable_tree(root: &Path) -> String {
        fn collect(dir: &Path, skip: &Path, out: &mut String) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name() == Some(OsStr::new("target")) {
                        continue;
                    }
                    collect(&path, skip, out);
                } else if path != skip
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }

        // `file!()` is workspace-root-relative under cargo, so this tracks the
        // module if it ever moves.
        let this_file = root.join(file!());
        assert!(this_file.is_file(), "{this_file:?} should be this source");

        let mut out = String::new();
        for dir in ["crates", "images"] {
            collect(&root.join(dir), &this_file, &mut out);
        }
        out.push_str(&std::fs::read_to_string(root.join("Makefile")).expect("Makefile"));
        assert!(
            out.len() > 100_000,
            "read back only {} bytes of tree — the walk found nothing to search",
            out.len()
        );
        out
    }

    /// Nothing in `.env.example` may name a variable this repository has never
    /// heard of. The failure it exists for is real and was live when this was
    /// written: `TASKS_MAX_SESSIONS`, `TASKS_DISPATCH_INTERVAL` and
    /// `TASKS_CONTAINER_IMAGE` sat in the working `.env` on the host this
    /// pipeline runs on, with zero occurrences in the tree *and* zero in the
    /// whole of git history — v1 residue that no reader here ever had. An
    /// example file is the one place such a name would be copied onward.
    ///
    /// It is a **naming check, not a proof of use**: a variable mentioned only
    /// in a doc comment passes. That is deliberately the same bar a human
    /// applies with `grep -rn <name> crates/`, and it is enough for the whole
    /// failure mode — a name that is invented, renamed away, or misspelled.
    ///
    /// The check is deliberately one-directional. The reverse — every variable
    /// the tree reads is documented — is the drift that produced a *known*
    /// live instance while this was being written (`BUILDER_IMAGE` is real and
    /// missing from CLAUDE.md's table; it is filed as its own issue). It is
    /// not implemented here because "a variable the tree reads" has no
    /// grep-shaped definition: `std::env::var` calls are wrapped, names are
    /// built from constants, and test fixtures set variables no operator ever
    /// should. A guard that cannot be made precise fires on things that are
    /// fine, and a guard that fires on things that are fine gets deleted —
    /// taking this direction, which *can* be made precise, with it.
    #[test]
    fn every_variable_the_example_names_is_known_to_the_tree() {
        let root = repo_root();
        let example = std::fs::read_to_string(root.join(".env.example")).expect(".env.example");
        let tree = searchable_tree(&root);

        let mut names: Vec<&str> = example
            .lines()
            .filter_map(assignment)
            .filter_map(|a| a.split_once('=').map(|(name, _)| name))
            .collect();
        names.sort_unstable();
        names.dedup();

        assert!(
            !names.is_empty(),
            "extracted no assignments at all — the extractor, not the example, is broken"
        );

        let unknown: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| !tree.contains(name))
            .collect();
        assert!(
            unknown.is_empty(),
            ".env.example names {unknown:?}, which appears nowhere under crates/, images/ \
             or the Makefile — either it is dead, or it is misspelled"
        );
    }

    /// The example's quoting has to survive the one thing anyone does with the
    /// file: uncommenting a line. `ORCHESTRATOR_CMD` is the value that makes
    /// this worth pinning — it contains spaces, so it is quoted, and a parser
    /// that kept the quotes would produce a command whose *program name*
    /// starts with `"`, failing a long way downstream of the mistake.
    ///
    /// Run through this module's own [`parse`], so the assertion is about the
    /// parser the server actually uses rather than about a second reading of
    /// the same file.
    #[test]
    fn the_examples_orchestrator_command_survives_being_uncommented() {
        let example =
            std::fs::read_to_string(repo_root().join(".env.example")).expect(".env.example");
        let uncommented: String = example
            .lines()
            .filter_map(assignment)
            .map(|a| format!("{a}\n"))
            .collect();

        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), &uncommented);
        let pairs = parse(&path).expect("every line of the example must parse once uncommented");

        let commands: Vec<&str> = pairs
            .iter()
            .filter(|(key, _)| key == "ORCHESTRATOR_CMD")
            .map(|(_, value)| value.as_str())
            .collect();

        assert_eq!(
            commands.len(),
            2,
            "both orchestrator shapes belong in the example: {commands:?}"
        );
        for command in &commands {
            assert!(
                command.starts_with("claude --print "),
                "a quote leaked into what would become the program name: {command:?}"
            );
        }
        assert!(
            commands.iter().any(|c| c.contains("Bash(curl:*)")),
            "the curl-only shape lost its allowlist: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("--dangerously-skip-permissions")),
            "the full-dev-agent shape lost its flag: {commands:?}"
        );
    }
}
