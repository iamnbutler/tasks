# Unbreak native Linux builds, and bound `GET /events?since=` in SQL

Two independent fixes, one per approved spec.

**#811 — scope the aarch64 cross-linker pin to the Makefile.**
`.cargo/config.toml` pinned `linker = "aarch64-unknown-linux-gnu-gcc"` for
`[target.aarch64-unknown-linux-gnu]`. Cargo's `[target.*]` keys are host-blind —
they fire whenever that triple is being built *for*, regardless of the host — so
on an aarch64 Linux host (every Scout and Builder VM, any Linux CI runner or
contributor) that triple is the host triple, and the pin applied to native
builds while naming a binary that only ships with the messense macOS
cross-toolchain. The failure was not at link time: it killed the first *build
script* (`libc`, `quote`, `proc-macro2`), so no `cargo build` or `cargo test`
in the repo could get off the ground. Because cargo config has no host-vs-target
conditional, no checked-in stanza for this triple can be correct on both hosts;
the file is deleted and the pin moves into the Makefile's three cross-compile
targets as `CARGO_TARGET_<TRIPLE>_LINKER`, which is the only layer that already
knows a build is a cross build. The env var name is derived from `LINUX_TARGET`
by cargo's own uppercase/underscore rule so the triple and the variable cannot
silently desync, and `check-toolchain` now guards the same `$(CROSS_LINKER)`
string by construction. This incidentally fixes `app-gpui`, which is excluded
from the workspace but lives under the repo root — cargo config discovery walks
upward, so the root file applied to it too.

**#810 — bound `GET /events?since=` in SQL.** `Store::events_since` fetched
every row from `since` forward and the handler discarded all but `limit`, so a
client backfilling by paging forward re-deserialized nearly the whole log on
every page — quadratic in log size, and the orchestrator's own prompt tells it
to run exactly that loop. The bound moves into the query (`... ORDER BY seq
LIMIT ?`) and `events_since` gains a `limit` parameter, mirroring the
`transcript_since(session_id, since, limit)` shape already in the same file. The
bind is `limit.max(0)` because SQLite reads a *negative* LIMIT as unbounded, so
passing a caller-supplied `-1` through would mean "the whole log" — the opposite
of what it looks like; clamping at the store keeps the bound from depending on
handler validation. The ~20 callers that passed `0` to mean "everything" now use
a new `all_events()`, so the one method that reads unbounded says so in its name
instead of leaving the footgun under the name the next handler author will reach
for. The response is byte-identical for every existing caller: no wire change,
no migration, no client change.

Verified on an aarch64 Linux host with **no env override of any kind** —
previously impossible: `cargo build --workspace --all-targets`, `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets` (zero warnings), and `cargo
test --workspace` at 285 passed / 0 failed. The pre-existing HTTP paging test
(`paging_events_reconstructs_the_log_that_newest_n_truncates`) passes untouched,
which is the evidence the wire behaviour did not move; the new store-level test
pins the page contract where the bound now lives, including the negative-LIMIT
trap. The Makefile cross path was exercised end-to-end by stubbing
`aarch64-unknown-linux-gnu-gcc` and `container` onto `PATH` — `check-toolchain`
passed, the pin was applied, and the binary was produced — and the pin was
confirmed to genuinely drive linker selection by setting it to a bogus value and
watching cargo reproduce the original build-script failure. Stubs and artifacts
were removed afterward. The genuine macOS→Linux cross build cannot be run from a
Linux host; that path is unchanged in substance (same linker binary, same
triple, same gate), but a reviewer on macOS should run `make images` once to
confirm.
