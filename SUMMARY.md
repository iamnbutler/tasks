# Preserved bundles: a listing, a place in the app, and a retention policy

A Builder VM is deallocated *before* the server pushes its branch, so when a
push or a PR is refused the thin git bundle the VM sent is the only copy of
that implementation anywhere. Since #891 those bundles are written to
`<scratch_root>/rejected/<build_id>.bundle` and never swept — but nothing
could see that directory and nothing ever emptied it. This adds three things.
A filesystem-backed service (`crates/tasks/src/bundles.rs`) that lists, stats
and deletes by reading the directory: the **filesystem is the only record**,
with no table, no migration and no cached size, because that directory is one
a human works in and a row asserting a file exists goes stale the moment
somebody `rm`s one. Three endpoints — `GET /bundles`, `GET
/builds/{id}/bundle` (404 as the ordinary answer, the `/sessions/{id}/notes`
shape) and a human-only `DELETE /builds/{id}/bundle`; a router with no bundle
service answers **503, never `[]`**, since "nothing was preserved" is the one
wrong answer to give about work that exists in exactly one place, and the
delete is refused to the orchestrator outright like `build-now` because there
is no undo. And a retention *policy* (`run::reclaim_bundles`, driven by
`Store::build_superseded`, run after each `poll_once` because that is where
superseding evidence arrives) that deletes a bundle only once every spec in
its batch has been carried by a **later build that succeeded** and every task
in it is **`done`** — never by age, never by disk usage. Both halves are
load-bearing: a later build that only opened a PR is not evidence, since
`watch_merges` can still find that PR closed unmerged and unwind the batch, at
which point the bundle is the head start again. One unreproduced spec keeps
the whole file, and "later" is `rowid` rather than `created_at` so a build
cannot supersede itself.

Around that: `Builder::preserve_bundle` now goes through the service and
announces itself as a `BundlePreserved` event, with `BundleRemoved` carrying
`superseded` — the whole difference between bookkeeping and somebody
destroying an implementation. `Services { briefings, github, bundles }`
replaces the positional service arguments to `router_with_services` /
`serve_on` / `serve_with_shutdown`, which is mechanical and reviewable on its
own. The app grows an amber "recovered implementation" block above the
inspector's actions for any task a bundle covers — size, age, the failure
reason, the `git fetch` verbatim and copyable, and a Delete that arms on the
first click and fires on the second — keyed to the *tasks* rather than the
build, because a build that never landed a branch has no PR and appears
nowhere else in the UI. Recovery is a printed command rather than a button on
purpose: the file is on the server host and the fetch runs in the human's own
checkout, and the bundle is thin, so `base_sha` is stated beside it. Docs
cover the endpoints, both event kinds and the policy (`docs/clients.md`, the
orchestrator system prompt, a new load-bearing rule in `CLAUDE.md`).

Verification: PASSED — `make test` (620 passed, 0 failed; the 6 LEAKs are the
documented scout/cancel timeout ones), doctests pass, `cargo clippy
--workspace --all-targets` clean, `cargo fmt` applied, `make app-check` and
`make app-test` (117 tests) pass. New coverage:
`crates/tasks/tests/bundles.rs` (the API's 404/403/503 shapes and all four
directions of the retention policy), the `bundles` module's own unit tests,
`byte_size`, `bundle_covering`, and `tests/builder.rs`'s rejected-egress test
now runs the *printed* recovery command against a fresh clone and asserts the
`BundlePreserved` event.
