# Allow `clippy::result_large_err` on `broker::authorize` (boxing it saves zero bytes)

`main` went red on the `fmt, clippy` CI job without a commit having changed it:
`stable` rolled to 1.98.0 and `clippy::result_large_err` now fires on
`BrokerState::authorize` in `crates/tasks/src/broker.rs`, which returns
`Result<Lease, axum::response::Response>`. It is the only site in the workspace,
and every Builder branch is cut from `main` and inherits the failure. This adds
an `#[allow(clippy::result_large_err, reason = ...)]` to that one function, with
the measurement that justifies it in the doc comment above it. One file, no
behaviour change: the signature, the body and both call sites are untouched.

It is `#[allow]` rather than the boxing the lint suggests because **boxing saves
nothing here**, and that is measured rather than assumed — I re-measured it on
this tree with a throwaway probe rather than trusting the spec: `Lease` is 136
bytes, `Response` is 128, and `Result<Lease, Response>` and
`Result<Lease, Box<Response>>` are **both** 136. The `Ok` variant already
dominates the enum, so boxing the error (or shrinking it to a small
`(StatusCode, &str)` denial) would move zero bytes of the thing the lint exists
to bound while adding a heap allocation to every denial. It is `#[allow]` rather
than `#[expect]` because the lint does not fire on 1.97.1, which is what the VM
images currently ship, so an `#[expect]` would be unfulfilled there and turn the
in-VM check red in order to silence one CI is complaining about.

## Evidence

The twelve lines are trivial; the two exit codes are the deliverable. I
installed `1.98.0 (88d9e12ae 2026-08-18)` — the exact build the failing run
used — into this VM and ran the exact CI line in the default message format
(not `--message-format short`, which hides lint names) on both toolchains,
against the unfixed tree as a control and against the final tree:

| `cargo clippy --workspace --all-targets -- -D warnings` | unfixed | fixed |
|---|---|---|
| under `+1.98.0` (what CI installs) | **exit 101** | **exit 0** |
| under `+stable` = 1.97.1 (what this VM ships) | exit 0 | **exit 0** |

The unfixed 1.98.0 run without `-D warnings` produces exactly one warning in the
whole workspace, at `crates/tasks/src/broker.rs`, naming
`the Err-variant is at least 128 bytes`. The bottom-left cell is the reason this
fix could not have been caught in a Builder VM: on 1.97.1 the unfixed code is
green, so the difference was never the flags, it was the compiler.

Also on the final tree: `cargo fmt --all -- --check` clean under both toolchains,
`cargo test -p tasks --lib broker` 16 passed, and `sh .tasks/verify`
(`make test-ci`, the gate the supervisor runs) exit 0.

## Review feedback

1. **Land it alone and do not widen it.** Done. One file, `crates/tasks/src/broker.rs`,
   and nothing else — no `rust-toolchain.toml`, no `clippy.toml`, no
   `.tasks/verify` edit, no reformatting, and neither follow-up anticipated. One
   deviation to name rather than let you find: the diff is **19 added lines, not
   twelve**, because item 2 below asked for the `#[expect]` reasoning in the code
   and that comment is five of them, with the rest being the size measurement in
   the doc comment. It is still one file and still only attributes and comments.
2. **Put the `#[expect]` reasoning in the code, not only in the spec.** Done — a
   five-line comment sits directly above the attribute saying that the lint fires
   on 1.98.0 and not on 1.97.1, that the images ship 1.97.1, and that `#[expect]`
   would therefore go red in the VM to silence a lint CI is complaining about. It
   is a `//` comment rather than doc text deliberately: it is addressed to the
   next reader of the diff, who is the person who would otherwise "fix" it, and
   not to a consumer of the API. The `reason = ` string carries the size half.
3. **Run the exact CI line one more time at the end.** Done, on the final
   committed tree, both toolchains, default message format — the table above.
   Both exit 0.

I made no change to the `#[allow]`-over-boxing decision, which you asked me not
to; I verified its numbers rather than restating them.

## Directions

- **Twelve lines in `crates/tasks/src/broker.rs` and no other file; stop and say
  why if you edit a second one.** No second file was edited. I did temporarily
  append a `size_probe` test to `broker.rs` to measure `Lease`, `Response` and
  the two `Result`s, then removed it — the committed diff is attributes and
  comments only, and the numbers it produced are quoted above and in the doc
  comment. Line count is 19 rather than 12; see review item 1.
- **`.tasks/verify` runs no clippy or fmt, so the in-VM gate cannot see this
  failure.** Confirmed by reading it — it is `set -e; make test-ci`. I ran it
  anyway (exit 0), which also leaves `target/` warm for the supervisor's own run,
  but it is not evidence about this fix and I did not treat it as such.
- **The images ship 1.97.1, so a clean local clippy proves nothing by itself;
  install 1.98.0, run the exact CI line under it and again under the VM's stable,
  in the default message format, and report both exit codes.** Done, and extended
  in one direction worth flagging: I also ran both lines against the **unfixed**
  tree, because two exit-0s on a fixed tree cannot distinguish a fix from a lint
  that never fired here. The 101 in the table is that control.
