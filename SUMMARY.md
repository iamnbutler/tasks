# Land #933's two dropped review items: the read-only agent shape, and `bind_socket`'s probe-to-bind race

Two documentation items required by the reviews behind #933 never reached a
Builder — review feedback did not reach one at all until #945 — so both were
unstarted work rather than reversed decisions. This lands them as prose. There
is no behaviour change, no rename, no new dependency and no migration; the
three files are `CLAUDE.md`, `crates/vm-pool/service/src/lib.rs` and
`crates/vm-pool/CLAUDE.md`, 86 added lines and nothing removed.

The first item records, in the root `CLAUDE.md` and immediately after the
*"Agent engine is Claude Code / the Agent SDK"* rule, the only worked example of
a read-only agent this repository ever had — `BRIEFING_CMD`, deleted whole with
`crates/tasks/src/briefing.rs` in #933. It keeps the four things that are not
derivable from the current tree: the command shape itself; that `--allowedTools`
is default-deny and *prefix-matched*, so the list is written in verbs
(`Bash(git log:*)`, `Bash(git diff:*)`) rather than in tools, because
`Bash(git:*)` would hand the agent `git push` and `git commit` along with the
log; that for an agent whose whole point is that it cannot write,
`--dangerously-skip-permissions` discards the allowlist rather than widening it;
and — the half that is genuinely unrecoverable, since `grep split_command` over
`crates/` now returns nothing — that a quoting-aware split is what makes such an
allowlist expressible at all, because `Bash(git log:*)` contains a space and
splitting on whitespace shatters it into two permissions that match nothing. It
names `orchestrator.rs:311`'s `split_whitespace` as the spawn path that cannot
express one, and ends with a recovery pointer into git.

The second names the residual window in `bind_socket`: the probe answers for
the instant it ran, and nothing holds that answer through the `remove_file` and
the `bind` after it, so two starts racing against the same stale path still
interleave and still leave one daemon on an unlinked inode. That is spelled out
in a comment directly above the unlink, and a paragraph is attached to the
rustdoc — specifically to the paragraph that describes the old displaced-daemon
failure in the past tense, which is the text a reader takes for "this is
closed". `crates/vm-pool/CLAUDE.md` gains the matching bullet immediately after
*"A live socket has an owner, and this process is not it"*, carrying the fix's
complete intended shape (an advisory `flock` on a sibling lockfile,
`LOCK_EX | LOCK_NB`, taken ahead of the probe, held for the process lifetime,
refusal mapped onto `AlreadyRunning`, **and the probe stays even then**, because
a lock cannot see an incumbent that predates it) and why it is a design rather
than a patch. The `flock` itself is deliberately not implemented here; the spec
flagged that choice for overruling and the reviewer upheld it.

## Review feedback

- **Required 1 — the recovery pointer does not reach what it recovers; name
  both files and say which holds which.** Done, and verified in this clone
  first, as the directions asked: `git show 63a1fb6^:crates/tasks/src/briefing.rs
  | grep -c DEFAULT_BRIEFING_CMD` returns `0`, and `git show
  63a1fb6^:crates/tasks/src/run.rs | grep -n DEFAULT_BRIEFING_CMD` returns line
  `102`. My clone agrees with the reviewer's, so the history is intact. The
  bullet now names `run.rs` for the command and the doc comment giving its
  reasons (`DEFAULT_BRIEFING_CMD`, `:102`) and `briefing.rs` for the splitter
  that makes it expressible (`split_command`, `:392`, test
  `split_command_groups_quoted_permissions`, `:439`), and says explicitly that
  `briefing.rs` holds no occurrence of the command at all.
- **Required 2 — scope the `--dangerously-skip-permissions` rule to read-only
  agents, or the file contradicts its own env table.** Done. I read the
  `ORCHESTRATOR_WORKDIR` row (`CLAUDE.md:1099`, which does say to pass the flag
  to run the orchestrator as a full dev agent) before writing the sentence. The
  rule is now scoped — *for an agent whose whole point is that it cannot write*,
  the flag discards the allowlist rather than widening it — and the bullet
  states in the same breath that the flag is not forbidden in general, pointing
  at that row. One source, not a better sentence.
- **Required 2, second half — say "default" of the orchestrator's allowlist.**
  Done: the bullet says the orchestrator's *default* command and cites
  `DEFAULT_ORCHESTRATOR_CMD`, `crates/tasks/src/run.rs:96` (confirmed at that
  line here). Nothing is asserted about what any deployment actually runs.
- **Attach the new rustdoc paragraph to the past-tense displaced-daemon
  paragraph, not merely somewhere in the doc comment.** Done — it is the next
  paragraph after it and opens by qualifying it ("That failure is closed against
  an incumbent that was already listening, and only against that").
- **Keep the documentation-over-`flock` choice; not overruled.** Kept. The
  complete intended shape of the `flock`, including the probe-stays clause, is
  written into `crates/vm-pool/CLAUDE.md`.
- **The items the reviewer confirmed as right** (not treating #933's commit
  message as evidence, leaving `docs/plans/2026-08-11-home-briefings.md`
  untouched, not rewording the ledger-discharge bullet, and putting a paragraph
  on the rustdoc as well as at the unlink) are all as approved. One correction
  of my own to the spec's fourth pitfall: the two tests it names are actually
  `a_stale_socket_is_reclaimed` and
  `a_live_socket_is_refused_and_its_owner_stays_reachable`, so the vm-pool
  `CLAUDE.md` bullet cites those real names rather than the spec's approximate
  ones.

## Directions

- **Prose and nothing else; the wording is the deliverable, not a paraphrase
  that keeps the facts and drops the reasons.** Followed. The diff is additive
  prose in three files, no code path touched. Each paragraph states the claim
  and then why the obvious alternative is wrong — why verbs rather than the
  tool, why the quoting is load-bearing, why `orchestrator.rs` is not the
  template, why narrowing the unlink window is not a fix.
- **Check both required changes yourself the same way they were found.** Done;
  both commands and their output are quoted above, along with the
  `ORCHESTRATOR_WORKDIR` row and `run.rs:96`. My clone did not disagree with the
  reviewer's on any of it, so there is nothing to raise loudly. I also confirmed
  `grep -rn split_command crates/` returns nothing, which is the claim the
  bullet's "no longer recoverable by reading the tree" rests on.
- **Do not assert what any deployment runs the orchestrator with.** Followed —
  see the third review item above.
- **Run the suite in the foreground; name what you ran if nextest is missing.**
  Followed; `cargo-nextest` is present and `make test` ran to completion in the
  foreground, doctests included. 7 tests reported LEAK — the documented
  scout-timeout ones (#958) the profile treats as pass. Nothing was chased,
  weakened or re-attributed, and no other failure occurred.

Verification: PASSED — `cargo fmt --all -- --check` clean, `cargo clippy --workspace --all-targets` clean, `make test` = 796 tests run, 796 passed, 0 failed (7 leaky, the documented scout-timeout ones), doctests included
