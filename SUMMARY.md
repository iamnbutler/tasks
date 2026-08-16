Classify a failure as a verdict or not, and only charge a strike for a verdict

`dispatch_attempts` and `build_attempts` exist so that work which genuinely
cannot be done stops consuming the pipeline after three tries. Until now a run
that died of something unrelated to the work was charged identically, so three
infrastructure deaths rejected a good task or `blocked` a good spec having
learned nothing — #825 burned five scout attempts in one night without a single
verdict among them. This adds `FailureClass` (`Verdict` | `Transport` |
`Cancelled` | `Orphaned`) to `tasks-protocol`, stamped by the **supervisor** —
the only thing that knows how the agent died, via a new
`AgentRun::failure_class()` reading `AgentEnding::is_transport()` — onto
`ScoutEvent::Failed`, `ScoutEvent::StoppedEarly` and `BuildEvent::Failed`, and
read by the host **off the field, never off the reason text**. Each dispatcher
gets one decision point (`ScoutError::failure_class` /
`BuilderError::failure_class` into the renamed `store::Strike::for_class`), and
the restart-orphan exclusion that used to live beside the accounting in
`run::record_outcome` is folded into the same classification rather than left as
a second mechanism. `Cancelled` is a variant rather than a special case, so
#876's cancel paths classify through the same rule they already short-circuit
around. Every waived strike appends a `Note` naming the class, the waiver reason
and the unchanged attempt count, because an attempt that was not spent is
otherwise indistinguishable from a cap that has been switched off.

What stays charged is deliberate, and the negative tests pin it: an agent that
ran to completion and produced nothing usable still burns its three, as does a
`Timeout` (the run had the entire budget), an OOM kill (a memory limit is a real
property of the work in that VM, and #828 exists so that death is legible as
itself) and every pre-agent setup failure (a clone against a base branch that is
gone fails identically every time; waiving it would retry forever with nothing
to stop it). `BuilderError::Egress` is classified `Verdict` as a judgement call
— a failed push happens *after* an implementation exists and is worth surfacing
against the batch — and it is a one-line change if reviewers disagree. Wire skew
is handled in both directions: `#[serde(default)]` covers an older supervisor
image omitting the field, and a hand-written `Deserialize` decays an *unknown*
class to `Verdict`, because a lost terminal event does not cost a strike, it
costs the run its outcome and hangs it until the deadline. Note the supervisors
only stamp the field once `make images` has rebuilt them; until then every event
defaults to `Verdict`, which is exactly today's behaviour. New host-side tests
drive the real supervisors with two new *stateless* api-death fixtures (the
scout-supervisor's own `stub-agent-api-death-always.sh` cannot be reused — it
asserts that attempt ≥2 carried `--resume`, which is false when the host
dispatches a fresh VM each time, and it would exit before writing any
stream-json, classifying `Verdict` and measuring the opposite of what it means
to). CLAUDE.md gains the rule beside the other pipeline invariants.

Verification: PASSED — `make test` (611 passed, 0 failed; the documented
scout-timeout LEAKs only), doctests, `cargo fmt --all`,
`cargo clippy --workspace --all-targets` clean, and `make app-check`
(app-gpui is untouched — nothing in `tasks-api` changed).
