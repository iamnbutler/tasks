# Adopt cargo-nextest and stop building binaries inside tests

The integration suites used to shell out to `cargo build` to locate the
binaries they exec — five or six times per test binary. Beyond the runtime,
those calls block on cargo's build-directory lock, so anything else holding it
(rust-analyzer's `cargo check`, a build in another terminal, an editor save
hook) stalls the run; measured with the lock held for 30s, one test binary
went from 2s to 31s, and `cargo test` runs binaries serially. That is the most
likely explanation for the suite occasionally taking minutes, and it is why
the slowness was never reproducible on demand. This removes every in-test
build: the two supervisor crates take their own binary from
`env!("CARGO_BIN_EXE_<name>")`, which cargo already builds and hands over,
and the `tasks` suite — which execs binaries from *other* packages — goes
through a new `common::workspace_bin(name)` that reads `TASKS_TEST_BIN_DIR`
(exported by `make test` after a prebuild) and only falls back to building,
memoized per binary, so a bare `cargo test --workspace` keeps working
unchanged. A stale export warns and builds rather than failing the suite.

Alongside that, `.config/nextest.toml` adds `default` and `ci` profiles and
the Makefile grows `test` (prebuild → nextest → doctests), `test-ci` (same on
`--profile ci`: no fail-fast, retries) and `test-cargo` (plain `cargo test
--workspace`, no prerequisites, and the thing that keeps the fallback path
exercised). The profile numbers are deliberate: the 5s slow-timeout is a
*visibility* threshold so `final-status-level = "slow"` actually names the
slow tail, with the kill threshold decoupled at 12 × 5s = 60s; and the leak
timeout stays at 1s because nextest waits the full period when a pipe is
really stuck — stretching it to 15s both costs 8 seconds of wall clock and
silently reclassifies a real leak as a pass. Both `make test` targets end with
`cargo test --doc --workspace` because nextest does not run doctests at all
and does not report them as skipped; `vm-pool-client` has two. Verified on
aarch64 Linux: 282 tests, all green on both nextest profiles and on `make
test-cargo`; `make test` is ~12s against ~22s for `cargo test --workspace`.
`cargo fmt --all --check` and `cargo clippy --workspace --all-targets` are
clean. CLAUDE.md documents the three targets, the nextest prerequisite, the
doctest gap, the expected LEAKs, and a new convention bullet: tests exec
binaries, they never build them.

Two things left alone on purpose. `crates/vm-pool/pool/tests/integration.rs`
still shells out to `cargo build` for `vm-pool-supervisor` — `TASKS_TEST_BIN_DIR`
must not leak into `crates/vm-pool/*`, which stays independently publishable,
so that wants a vm-pool-local helper with its own env var as a follow-up in
that crate. And `.cargo/config.toml` pins `linker =
"aarch64-unknown-linux-gnu-gcc"` for `aarch64-unknown-linux-gnu`, which is the
macOS cross-toolchain name; on a Linux aarch64 host that is the *host* triple,
so every cargo build in this repo fails with "linker not found" there. It is
correct as written for the intended macOS dev machine and out of scope here
(worked around locally with
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc`), but it means Linux CI or
a Linux contributor cannot build today and deserves its own issue. There is no
`.github/workflows` in this repo yet; when one lands it should call
`make test-ci`, which already includes the doctest pass.
