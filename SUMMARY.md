# Brief's in-flight count no longer double-reports the specs a build is carrying

`Brief::pipeline` listed every queued/running build with the issues it carries,
then counted every `spec_queue` row still marked `approved` — and `create_build`
deliberately leaves that status alone (so a failed build can return its specs for
another attempt), which meant the same specs appeared on both lines. A real brief
read `build_4b5afedba455 is running (#827, #826, #824)` directly above `3 approved
spec(s) are waiting to be batched`. This applies the exclusion that already exists
twice elsewhere: the spec ids of the in-flight builds are collected inside the same
loop that renders their lines, and subtracted from the count. Building the set from
the printing iteration rather than a second traversal makes it structurally
impossible for the set and the lines to disagree, and `queued` counts as carried
alongside `running` because builds are serial — a batch waiting behind the running
one has already been asked for. That is the same rule `Store::obligations` and the
guard in `Store::create_build` enforce.

The line is also reworded to `N approved spec(s) are in no build yet, waiting to be
batched into a Builder run`, so a reader can tell the two populations apart without
knowing the implementation; the `if approved > 0` guard stays, so a fully-dispatched
pipeline drops the line rather than printing zero. Severity here is credibility
rather than correctness — an orchestrator that believed the old line and posted
`POST /builds` got a 400 from the existing "already part of build" guard, so the
write layer was never at risk. What was at risk is the reason briefs exist: adjacent
lines that contradict each other teach their reader to re-derive everything, which
costs exactly the context the brief was built to save. Behaviour-only change in one
function plus an integration test (`crates/tasks/tests/brief.rs`) that walks two
approved specs through dispatch, claim, and a second dispatch, asserting the count
drops as each is picked up and the line disappears when nothing is left. The test
fails on the unpatched source with the reported symptom. No schema, API, or wire-type
change; full workspace suite, `cargo fmt --all`, and `cargo clippy --workspace
--all-targets` are clean.
