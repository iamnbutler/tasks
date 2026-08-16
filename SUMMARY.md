Agent processes have been dying intermittently at around 380 seconds elapsed,
across both scouts and builders, and nothing in this repository can prevent it:
the connection drops mid-response, below the agent, in the VM's network path —
which is why two scouts started one second apart in the same image on the same
host both crossed the boundary and only the one that was mid-generation died.
What *can* be fixed is that the drop currently costs the whole run. Claude Code
sessions are resumable by id, and the id is already in the stream-json the
supervisor is forwarding, so a supervisor that watches its agent die of
`terminal_reason: api_error` now re-invokes it with `--resume <session_id>` in
the same VM — same conversation, same worktree, same `NOTES.md` — and the run
continues instead of ending. Resuming happens in the supervisor and never as a
host-side re-dispatch, because a re-dispatch gets a new VM and a fresh clone;
for a Builder that difference is the implementation itself, and the new builder
test proves it by committing half the work before the death and the other half
after.

The classification and every guard live in one new pure module,
`crates/tasks-protocol/src/agent_run.rs`, beside `vm_memory`: `ResultWatcher`
rides the same stdout loop that emits `Progress`, so what it classifies is
byte-for-byte what was reported; `decide` applies six guards, each with its own
named `NoResume` reason, so a resume that *didn't* happen is as legible as one
that did. The guards are the load-bearing part, because the failures you must
not retry look superficially like the one you must — an OOM kill would meet the
same memory limit with a larger conversation, a missing terminal record means
the host is deallocating this VM right now, a plain-text agent has no
postmortem to be missing, and a command carrying `--resume`/`-r`/`--continue`/
`-c`/`--session-id`/`--fork-session` already belongs to the operator. Backoff
rises (2s / 15s / 30s) because a per-connection lifetime cap is gone the instant
it fires, but a 429 or 529 wants time. Terminal reasons now name the transport
failure instead of only its symptom: "SPEC.md not found" and "agent produced no
commits" read as verdicts on work that was never judged. Both supervisors gain
`SCOUT_MAX_RESUMES` / `BUILDER_MAX_RESUMES` (default 2, `0` disables), set in
`images/{scout,builder}/Dockerfile` because they are read inside the VM — so
**the images must be rebuilt**; a server upgrade alone changes nothing. Three
new integration tests carry the behaviour with real supervisor processes and
shell-script agents, and each fixture asserts on its own side that it was handed
the session id it announced, since a resume aimed at the wrong conversation
would look like a recovery and behave like a restart. One thing this does not
close: `dispatch_attempts` is still charged for an infrastructure death, so
three runs that each exhaust their resumes can still reject a task for a reason
unrelated to the work; the recommended shape (a `serde(default)` classification
field on the failure events, carried into `record_outcome` beside the existing
`is_disconnect` precedent — never a string match on the reason text) is written
up in the issue and in CLAUDE.md.
