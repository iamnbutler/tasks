# `GET /version` and a connect-time build check, so a stale client says so

The server now publishes its build identity — `0.1.<commit count>`, the short
SHA (`-dirty` for an uncommitted tree), and `min_client_version` — on a cheap,
unauthenticated, store-free `GET /version`, first in the router. `tasks-client`
preflights that route on connect and exposes a `Preflight` verdict whose
`warning()` is the one line a UI shows: "This client build (0.1.120) is older
than the server supports (needs 0.1.140, server is 0.1.163) — rebuild the
client (`make app`)." Before this, a client from a different commit than the
server failed as a pile of unrelated decode errors — a strict enum that didn't
know a variant, a field that wasn't there — which reads as "the server is
broken" when the fact is "your app is old". Under-minimum clients are **warned,
never refused**: every route keeps answering, because in a single-user system
where both ends ship from one tree the value is the diagnosis, and a 426 on
every route would turn one legible sentence back into the wall of failed
requests this exists to replace. Version comparison is numeric and
component-wise, not lexical (`0.1.100` beats `0.1.9`), and a build with no git
in reach is `Indeterminate` and warns about nothing — a warning that fires on
merely unidentifiable builds gets trained out of use. `MIN_CLIENT_VERSION` is
`0.1.0` and moves by hand, only for an actual wire break; a unit test asserts
it is never *ahead* of the running build, which is the tell that it was raised
as a ratchet rather than for a break.

The identity scheme was extracted from `app-gpui/build.rs` into a new
dependency-free `build-stamp` crate (`emit(prefix)` from a `build.rs`), so the
server, the client and the About window are one implementation rather than
three copies that drift — comparing these numbers across processes only means
anything if one thing computes them. `app-gpui/build.rs` is now five lines with
identical emitted names, so `make app`'s `TASKS_GPUI_VERSION` /
`TASKS_GPUI_COMMIT` pins still work; `crates/tasks/build.rs` keeps its
load-bearing `rerun-if-changed=migrations` line alongside build-stamp's (any
`rerun-if-changed` replaces cargo's default package-wide watch, so both sets
have to coexist). The app sets `with_client_version(about::VERSION)` so the
warning names the number About shows, runs `preflight()` in the background on
every `Connected` (a reconnect is usually a server that restarted into a new
build), and renders the result above the generic error banner. A 404 from
`/version` is a verdict, not an error — that server predates the route, so it
is the stale one — and only transport failures are `Err`. Tests cover the route
over real HTTP and its flat three-string body, the four preflight verdicts
against real servers on real ports, and the comparison's digit-boundary bug.
Docs: a "Check the build on connect" section in `docs/clients.md`, and
`build-stamp` in the CLAUDE.md structure list.

`GET /version` is deliberately state-free so it can answer while the rest of
the process is still opening, which makes it the natural liveness/identity poll
for the restart swap (#843): "is the new process up, and is it the build I just
made?" in one request. Deliberately out of scope: there is no
`min_server_version` and no warning for the reverse skew (a client newer than
the server) — the 404 is the only reverse-direction signal today, though
`Preflight::server()` vs `client_version()` already has the data. Verified with
`cargo test --workspace` (all green, including doctests), `cargo clippy
--workspace --all-targets` and `cargo fmt --all --check`, both clean.
`cargo-nextest` isn't installed in this environment, so `make test-cargo` — the
documented fallback — is what was run. `app-gpui` can't be compiled on Linux
(macOS gpui stack, excluded from the workspace on purpose): its `build.rs` was
verified by compiling it standalone against the `build-stamp` rlib and running
it (in-repo it prints `0.1.510` / `ab53258-dirty`; outside a checkout it falls
back to the crate version and `unknown`; the env overrides win), but the three
`src/` edits — a field, a `check_build` method modelled on `refresh`, and one
`else if` in the banner chain — are pattern-matched against surrounding code
and not compiler-verified. Whoever has a Mac should run `make app` once.
