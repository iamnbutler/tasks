# Contributing

Short, because most of it is ordinary. What follows is the part you cannot
guess from the tree and will otherwise get wrong.

## Read `CLAUDE.md` first

It is not agent boilerplate. It is the actual specification of this system —
every load-bearing design rule, each one written with the failure that
produced it. Nearly every review comment you would otherwise receive is a
paragraph in there. If a change contradicts one of those rules, the change
needs to move the rule *and say why*, in the same commit.

## Before you commit

```sh
cargo fmt
cargo clippy --workspace --all-targets   # clean, no warnings
make test
```

## The unusual things

**Tests exec binaries; they never build them.** A `cargo build` inside a test
blocks on the build-directory lock, so a background `cargo check` — your
editor's rust-analyzer, another terminal — stalls the whole suite. Use
`make test`, which prebuilds the supervisors and exports `TASKS_TEST_BIN_DIR`.
Inside a test, reach for `env!("CARGO_BIN_EXE_<name>")` for a binary in the
same package and `common::workspace_bin(name)` for one from another.

**Some tests report LEAK, and that is expected.** The three scout-timeout
tests leave a stray child holding the output pipe. `.config/nextest.toml` sets
`leak-timeout` to `result = "pass"`; it looks like a failure in the output and
is not.

**nextest does not run doctests** — silently, with no skip count. Both
`make test` and `make test-ci` end with `cargo test --doc --workspace`.
Anything else that runs the suite must too.

**Migrations are named for a UTC instant, never for the next free number.**

```sh
make migration NAME=build_transcripts
```

Do not hand-roll one by copying its neighbour and adding one. That number is
read off a tree that cannot see its sibling branches, so two branches pick the
same one, the collision exists only after the merge, and it surfaces as a boot
failure in a process that has already taken the port.

**`app-gpui` is not a workspace member**, so `make test` does not touch it. It
*does* compile and test on Linux, which was long assumed otherwise:

```sh
make app-check   # cargo check --all-targets
make app-test    # its own unit tests
```

Neither needs a display or a Mac. What does need a Mac is running it: whether
a pixel landed, whether a menu item is actually greyed, whether the title bar
sits right next to the traffic lights. That is `make app`, and if your change
touches layout, say in the pull request that you could not look at it.

**`make images` is the deployment step for anything inside a VM**, and it
needs a Mac with apple/container and the cross toolchain. A fix to
`crates/scout-supervisor`, `crates/builder-supervisor` or anything under
`images/` reaches nothing — not a test, not the pipeline — until someone runs
it. Merging is not deploying. Say so in the pull request.

**Real processes, real SQLite, no mocks.** HTTP tests bind real servers on
`127.0.0.1:0`. `crates/vm-pool/` has its own `CLAUDE.md` and its own
conventions, and it must never depend on a tasks crate — it stays
independently publishable, and app vocabulary enters it only through the
`AppProtocol` generic.

## Pull requests

Rust edition 2024. `thiserror` enums per module, `tracing` for logging.

Explain the *failure*, not the diff — this codebase's commit messages and doc
comments are written so the next person meets the reason before the code, and
that is the register to match. If you fixed something subtle, the best thing
you can put in the pull request is the way you convinced yourself it was
fixed: which test fails if the change is reverted.

This repository merges some of its own pull requests, autonomously, under the
charter described in the README. Your pull request is reviewed by a human.
