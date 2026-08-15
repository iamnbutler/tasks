# vm-pool tests stop shelling out to `cargo build`

vm-pool's tests exec a real supervisor binary from a sibling package (no mocks
— that is the house rule inside `crates/vm-pool/`), and they got its path by
running `cargo build` inline. There were three copy-pasted `build_supervisor`
helpers — in `pool/tests/integration.rs`, `pool/src/lib.rs`, and
`pool/src/transport.rs` — called from ten sites, none of them memoized. Since
even a no-op `cargo build` takes cargo's build-directory lock, each of those
calls was a place the suite could stall behind rust-analyzer, an editor save
hook, or a build in another terminal. This replaces all ten with a new
dev-dependency-only crate, `vm-pool-test-support`, whose `supervisor_binary()`
prefers a prebuilt binary from `$VM_POOL_TEST_BIN_DIR` and otherwise builds at
most once per test process. The memo is a `OnceLock<Mutex<HashMap<…>>>` and the
mutex is deliberately held *across* the build, so a second test wanting the
same binary waits on the first build instead of starting its own. It uses
`$CARGO` rather than `cargo` from `PATH` so a non-default toolchain stays
consistent with the outer `cargo test`, and it checks the binary name before
the package name — the supervisor package declares `[[bin]] name =
"supervisor"`, so `VM_POOL_TEST_BIN_DIR=$PWD/target/debug` works with no
copying. A set-but-stale directory warns and degrades to a build rather than
failing the suite.

The env var is vm-pool-local by design rather than reusing the tasks-side
`TASKS_TEST_BIN_DIR`: vm-pool is vendored infrastructure that must stay
independently testable and publishable, so nothing from the surrounding
workspace leaks in. On the tasks side, `make test-bins` gains `-p
vm-pool-supervisor` and `make test` / `test-ci` export both variable names at
the same directory (merged into the existing targets rather than replacing
them); `make test-cargo` deliberately exports neither, keeping the
build-on-demand fallback exercised. The `test-support` dependency is path-only,
so `cargo package -p vm-pool-manager` strips it from the published manifest —
verified, the packaged `Cargo.toml` still lists only `tempfile`.

Verified behaviourally, not just by reading. With `VM_POOL_TEST_BIN_DIR` set
and `CARGO=/nonexistent-cargo`, both vm-pool test binaries pass (50 + 5 tests),
which is only possible with zero shell-outs. With the variable unset and
`CARGO` pointed at a counting wrapper, each binary invokes cargo exactly once,
down from five. Under a 15-second `flock` on `target/debug/.cargo-lock`, the
pool integration binary takes 1s prebuilt versus 14s falling back. Full
workspace suite is green (325 tests, 0 failures), as is a bare `cargo test -p
vm-pool-manager` with nothing exported; `cargo fmt --all` and `cargo clippy
--workspace --all-targets` are clean.

One note for reviewers: the spec was written before #801 landed and assumed
`common::workspace_bin`, `TASKS_TEST_BIN_DIR`, and `make test-bins` did not yet
exist. They do now, so this merges into those targets instead of adding
competing ones. The two helpers stay separate on purpose, and the root
`CLAUDE.md` convention now says so. The spec's `.cargo/config.toml` aarch64
linker pitfall is also already resolved — that pin moved into the Makefile.
`find_supervisor_binary()` in `pool/src/transport.rs` is production code with a
similar name and was left alone.
