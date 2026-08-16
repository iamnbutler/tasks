# Multi-repo: per-repo project status on the server, a repo switcher in the app

The pipeline was single-repo in practice but not in schema, and less of the
server was missing than it looked: `POST /projects`, `GET /projects` and
`resolve_project` already existed, and every GitHub-writing body already carried
an optional `project_id` the server refuses to guess at. What was genuinely
missing was a way to *stop* working on a repo, and any client surface at all.
This adds one additive column — `projects.status` ∈ `active | paused |
archived`, the honest per-repo counterpart of the global `/mode` — and wires it
into the three places that select work: scout dispatch (`next_dispatchable`
skips a project that does not `dispatches()`, and skips *over* it so the queue
behind a paused repo still moves), the build claim (checked **inside**
`claim_next_queued_build`'s transaction, because claim-then-release would flip a
build `queued → running → queued` every tick and drag its batch's tasks to
`building` with it), and issue intake (only the **upsert** is skipped — closure
is only ever learned from absence in the open set, so an archived repo is still
fetched and reconciled, or a task with a Builder PR open would be stranded at
`gh_state = open` with nothing to make it loud). `POST /projects/{id}/status` is
the new endpoint; `resolve_project` stops counting archived projects, so
archiving one does not break `POST /issues` for the repo that is left, while
naming an archived project explicitly still resolves it. There is deliberately
no delete — `decisions` is append-only and `tasks.project_id` cascades, so
deleting a project would take the audit trail the charter rests on with it — and
both project writes are **human-only, refused to the orchestrator outright** on
the `build-now` precedent: they decide what the pipeline is pointed at rather
than doing work inside it, and no charter capability describes that. Mode stays
global, deliberately: the dispatcher's real constraints are server-wide (one
`SCOUT_MAX_CONCURRENT`, one serial build lane, one vm-pool), so a per-repo
`play` could not run while another repo's build held the lane; a new store test
pins that argument in code. Duplicate repos are now refused **case-
insensitively** in both add paths, because `UNIQUE(repo_owner, repo_name)` is
case-sensitive and `Owner/Repo` beside `owner/repo` is two projects for one
repository — checked in code rather than with a unique index, since that
migration could fail on a database that already holds the duplicate.

In `app-gpui`, the title bar's repo label becomes a switcher (a popover, not a
menu-bar menu: `set_menus` leaks a boxed action per item per rebuild, and the
item list here *is* the project list), with per-repo Pause/Archive/Resume, an
Add Repo window, and a File ▸ Add Repo… item so the control is reachable before
the first snapshot renders anything. The filter is a client-side view filter
over one working set — `GET /tasks` is shared with the orchestrator, the
briefing generator and `tasks status` — and the Queue section is deliberately
*not* filtered by it, because its reorder endpoints are bulk replaces over the
global ordering and a narrowed list would rewrite the ranks of repos the human
cannot see; rows name their repo instead, and only when the rows on screen
disagree about it, which keeps a single-repo window pixel-identical to the one
before multi-repo. The issue composer now states the repo it will file into and
carries the `project_id` verbatim instead of asking the orchestrator to pick,
refusing to send only in the one case the server also refuses: several repos in
view and none selected. Every switcher decision that is decidable without a
pixel is a pure function in the new `projects.rs` and unit-tested there.
`docs/clients.md`, the orchestrator's system prompt and `CLAUDE.md` record the
new endpoints and the two refusals.

Verification: PASSED — `make test` 643 passed / 0 failed (6 leaky: the
documented scout-timeout and cancel tests), `cargo clippy --workspace
--all-targets` clean, `cargo fmt --all --check` clean, `make app-check` clean,
`make app-test` 137 passed, and `app-gpui`'s own `cargo fmt --check` clean. The
GUI was compiled and unit-tested but never run: per CLAUDE.md that boundary is
real, so where the popover lands under the title-bar label, how the switcher's
rows read at 240px, and whether the disabled "File issue" button looks disabled
are still `make app` on a Mac.
