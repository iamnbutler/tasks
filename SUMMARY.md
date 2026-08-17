# Two channels that did not exist: directing an agent, and seeing what its image is running

**`directions` — one labelled channel for telling an agent what to do (#917).**
A `rationale` explains a judgment to whoever reads the `decisions` ledger
afterwards and reaches no VM ever, so an instruction put there is one the agent
never sees. There was no other channel, which is why `rationale` got reached
for — it was the only free-text field on the request. (One premise of the issue
is corrected here: the Builder was never directed through `rationale`; there
was no interpolation code to remove, because there was no Builder direction
channel at all.) `Directions { text, author }` is that channel. It reaches a
Scout or a Builder as its own labelled prompt section — after the field notes,
after the specs, immediately before `## Your job` — carries its author so the
prompt can name them and a Builder can see it is not reading a Scout, and is
persisted against the *run* that carried it rather than only the task. Both
sections demand every direction be accounted for in the run's own artifact
(`SPEC.md`'s `### Notes`, `SUMMARY.md`), declines included, and say that a
genuine conflict resolves in the directions' favour but must be *stated*, since
the reviewer cannot see this section. An undirected prompt grows no heading at
all. Staged scout directions are sticky rather than consumed — a VM death or a
`needs_revision` return would otherwise leave the retry unaimed with nobody
noticing — which is also why "absent" cannot mean "clear", and oversized
directions are a 400 rather than a truncation. Reachable from `POST /builds`,
`POST /tasks/{id}/queue | /scout`, `POST /tasks/{id}/build-now` (where they
stay strictly out of the spec that call authors) and `tasks-client`; no GUI
affordance, which is deliberate and noted in the spec. While here, the Builder
preamble's provenance claim is fixed: it told every Builder its specs had been
explored "by implementing it once in a throwaway branch" and to trust their
pitfalls, which is false for a hand-authored `build-now` spec and was the
strongest trust claim in the prompt made about the artifact with the least
behind it. It is now per spec, branching on `Spec::session_id`.

**Supervisor stamping, and reporting what the images are running (#909).** The
pipeline installs fixes to its own supervisors only when a human runs `make
images`, and nothing noticed that nobody had — #888's fix for a dropped API
connection sat on `main` for ten hours while that exact failure killed a
52-minute scout inside an image built before it, and the old supervisor, having
no idea it was old, charged a dispatch strike for a run that judged nothing.
Both supervisors are now stamped by `build-stamp` exactly as the server is (one
implementation, which is the only reason the two numbers are comparable), each
answers `--version` and states its identity on the `Started` event of its
protocol — the only moment there is to ask, since a VM exists only while a run
is inside it — and the host records it per image and reports it from `GET
/status`, `tasks status`, the Server window and the orchestrator's brief. Three
things are load-bearing: the new field is `#[serde(default)]` on both events
and on `ServerStatus.images`, because images are upgraded by hand so the host
is routinely newer than the supervisor talking to it (that skew *is* the bug)
and because `tasks reload` decodes the *old* server's `/status` before it
swaps; absence is the loudest reading rather than the quietest (`Unstamped`,
never `Unknown`); and the verdict is never stored, only computed at read time
against the running server's build, since the server is replaced far more often
than the images are. Nothing observed reports an empty list and every renderer
says "none observed yet" rather than "current". `make images-check` boots each
image and reads `--version` back, covering the one window the observation
cannot — right after a rebuild, before anything has run — and `make images`
ends by invoking it. The autonomy question is answered "no" and written into
CLAUDE.md: a merge does not rebuild images, because nothing inside the pipeline
can reach the cross toolchain, the `container` CLI or the checkout a rebuild
needs, and there is no `ObligationKind::StaleImage` because an obligation the
orchestrator can never discharge is how a signal gets trained out of use.

Two notes for whoever lands this. **The images must be rebuilt for the second
half to show anything** — until `make images` runs on a Mac with
apple/container, `unstamped` / "PREDATES STAMPING" is the correct reading, not
a bug; it is the feature reporting the state #909 was filed about. And
`images-check` cannot run in an agent VM, so it was exercised here against a
`container` shim on `PATH` for all three outcomes (current, older stamp,
pre-stamping output), exiting non-zero on the latter two; the real
supervisors' `--version` output was checked directly. The whole stamping chain
*is* covered by the suite, in the two integration tests that run the real
supervisor binaries end to end (`tasks::scout
scout_dispatch_end_to_end_produces_spec` and `tasks::builder
a_batch_of_two_specs_lands_as_one_branch_and_one_pr`) — every other test would
pass just as well with the field silently dropped, because `Option` plus
`serde(default)` is by design indistinguishable from "not sent".

Verification: PASSED — `make test-ci` (671 passed, 6 leaky as documented, 0 failed; doctests included), `cargo clippy --workspace --all-targets` clean, `cargo fmt --all --check` clean, `make app-check` and `make app-test` (176 passed) green, `cargo fmt --check` clean in app-gpui, and `make images-check` exercised against a `container` shim for all three outcomes.
