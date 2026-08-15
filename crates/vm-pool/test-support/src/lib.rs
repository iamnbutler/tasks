//! Test-only helpers for vm-pool.
//!
//! vm-pool's tests exec a real supervisor binary — no mocks, see
//! `crates/vm-pool/CLAUDE.md`. That binary lives in a sibling package, so
//! `CARGO_BIN_EXE_*` is not available to the tests that need it, and the
//! obvious fallback (`cargo build` inline) takes cargo's build-directory
//! lock: every call is a place the suite can stall behind rust-analyzer, an
//! editor save hook, or a build in another terminal.
//!
//! So: prefer a prebuilt binary from `$VM_POOL_TEST_BIN_DIR` (populated by
//! `make test-bins`), and otherwise build at most once per test process.
//!
//! This crate is dev-dependency-only and deliberately vm-pool-local — its own
//! env var, no dependency on the surrounding tasks workspace — so it travels
//! with vm-pool when vm-pool is extracted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Environment variable naming a directory of prebuilt test binaries.
///
/// Exported rather than written as a literal in the Makefile's sibling docs
/// so the name is greppable from both ends.
pub const BIN_DIR_ENV: &str = "VM_POOL_TEST_BIN_DIR";

/// Path to the vm-pool supervisor binary — the common case.
pub fn supervisor_binary() -> PathBuf {
    test_binary("vm-pool-supervisor", "supervisor")
}

/// Path to a binary from another workspace package.
///
/// `package` is the cargo package name; `bin` is the `[[bin]] name`, which is
/// what cargo actually writes to disk (for the supervisor those differ:
/// package `vm-pool-supervisor`, binary `supervisor`).
///
/// Resolution order: `$VM_POOL_TEST_BIN_DIR/<bin>`, then
/// `$VM_POOL_TEST_BIN_DIR/<package>` (so a directory populated by copying
/// under the package name works too), then a real `cargo build`.
pub fn test_binary(package: &str, bin: &str) -> PathBuf {
    static BINS: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

    let key = format!("{package}/{bin}");
    // The lock is held across the build on purpose: a second test wanting the
    // same binary should wait on the first build rather than start its own.
    let mut cache = BINS.get_or_init(Default::default).lock().unwrap();
    if let Some(path) = cache.get(&key) {
        return path.clone();
    }

    let path = match std::env::var(BIN_DIR_ENV) {
        Ok(dir) => lookup_in(Path::new(&dir), package, bin).unwrap_or_else(|| {
            // A stale or typo'd export degrades to a build rather than
            // failing the suite — but says so, so it doesn't silently
            // reinstate the stall it was meant to remove.
            eprintln!("warning: {BIN_DIR_ENV}={dir} has no {bin}; building {package}");
            cargo_build(package, bin)
        }),
        Err(_) => cargo_build(package, bin),
    };

    cache.insert(key, path.clone());
    path
}

/// The `$VM_POOL_TEST_BIN_DIR` half of [`test_binary`], factored out so it is
/// testable without mutating the environment (`set_var` is `unsafe` in
/// edition 2024, and unsound with libtest running tests on threads).
fn lookup_in(dir: &Path, package: &str, bin: &str) -> Option<PathBuf> {
    [dir.join(bin), dir.join(package)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Build a package and pull its executable path out of cargo's JSON output.
///
/// Uses `$CARGO` rather than `cargo` from `PATH` so a non-default toolchain
/// stays consistent between the outer `cargo test` and this inner build.
fn cargo_build(package: &str, bin: &str) -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args(["build", "-p", package, "--message-format=json"])
        .output()
        .expect("run cargo build");
    assert!(
        output.status.success(),
        "cargo build -p {package} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|msg| {
            if msg.get("reason")?.as_str()? == "compiler-artifact"
                && msg.get("target")?.get("name")?.as_str()? == bin
            {
                Some(PathBuf::from(msg.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("no {bin} executable in cargo build -p {package} output"))
}

/// Set on the re-exec'd child of
/// `a_populated_bin_dir_never_shells_out_to_cargo`, to stop it recursing.
#[cfg(test)]
const NO_CARGO_CHILD: &str = "VM_POOL_TEST_NO_CARGO_CHILD";

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of this crate, stated so it can fail.
    ///
    /// Every other test here checks that `lookup_in` *prefers* a prebuilt
    /// binary. None of them can fail if resolution quietly builds anyway —
    /// which is the regression that matters, because a `cargo build` takes
    /// the build-directory lock and reinstates the stall this crate exists to
    /// remove. So: point `$CARGO` at something that cannot exec, and assert
    /// resolution still succeeds. Any reach-through to `cargo_build` dies
    /// loudly instead of costing seconds nobody attributes.
    ///
    /// Re-execs this test binary rather than setting the variables in-process:
    /// the environment is process-global, libtest runs tests on threads, and
    /// `set_var` is `unsafe` in edition 2024 for exactly that reason.
    ///
    /// The parent resolves the binary the ordinary way first — which may build
    /// once, memoized, as designed — so the test is meaningful under plain
    /// `cargo test` as well as under `make test`, where the directory is
    /// already exported.
    #[test]
    fn a_populated_bin_dir_never_shells_out_to_cargo() {
        if std::env::var(NO_CARGO_CHILD).is_ok() {
            // The child: `$CARGO` is poison and the bin dir is populated.
            let path = supervisor_binary();
            assert!(
                path.is_file(),
                "resolved {} which is not a file",
                path.display()
            );
            return;
        }

        let binary = supervisor_binary();
        let dir = binary.parent().expect("binary has a parent directory");

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::a_populated_bin_dir_never_shells_out_to_cargo",
                "--nocapture",
            ])
            .env(NO_CARGO_CHILD, "1")
            .env(BIN_DIR_ENV, dir)
            .env("CARGO", "/nonexistent-cargo")
            .output()
            .expect("re-exec the test binary");

        assert!(
            output.status.success(),
            "resolution shelled out to cargo despite a populated {BIN_DIR_ENV}\n\
             stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn lookup_prefers_the_bin_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("supervisor"), b"").unwrap();
        std::fs::write(dir.path().join("vm-pool-supervisor"), b"").unwrap();

        assert_eq!(
            lookup_in(dir.path(), "vm-pool-supervisor", "supervisor").unwrap(),
            dir.path().join("supervisor"),
        );
    }

    #[test]
    fn lookup_falls_back_to_the_package_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vm-pool-supervisor"), b"").unwrap();

        assert_eq!(
            lookup_in(dir.path(), "vm-pool-supervisor", "supervisor").unwrap(),
            dir.path().join("vm-pool-supervisor"),
        );
    }

    #[test]
    fn lookup_misses_an_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(lookup_in(dir.path(), "vm-pool-supervisor", "supervisor").is_none());
    }

    #[test]
    fn lookup_ignores_a_directory_of_the_right_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("supervisor")).unwrap();
        assert!(lookup_in(dir.path(), "vm-pool-supervisor", "supervisor").is_none());
    }

    /// The end-to-end check: whichever path this resolves through, it yields a
    /// real file, and asking twice gives the same answer without a second
    /// build. With nothing exported this performs one real `cargo build`.
    #[test]
    fn supervisor_binary_is_memoized() {
        let first = supervisor_binary();
        assert!(first.is_file(), "{} is not a file", first.display());
        assert_eq!(first, supervisor_binary());
    }
}
