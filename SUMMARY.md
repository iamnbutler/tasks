# Multi-repo: project status on the server, a repo switcher in the app

The pipeline was single-repo in practice but not in schema, and rather less of
the server was missing than it looked: `POST /projects`, `GET /projects` and
`resolve_project` already existed, and every GitHub-writing body already
carried an optional `project_id` the server refuses to guess at when more than
one project exists. What was genuinely missing was a way to *stop* working on a
repo, and any client surface at all. This adds one column —
`projects.status` ∈ `active | paused | archived`, the honest per-repo
counterpart to the global `/mode` — and wires it into the three places that
select work: scout dispatch (`next_dispatchable` `continue`s past a repo that
does not dispatch, so a paused repo at the head of the queue never starves the
ones behind it), the build claim (a `WHERE p.status = 'active'` *inside*
`claim_next_queued_build`'s transaction, because claim-then-release would flip a
build `queued → running → queued` every tick and drag its batch's tasks to
`building` each time), and the **upsert half only** of the poll — an archived
repo is still fetched, still reconciles closures and still has its merges
watched, because closure is only ever learned from absence in the open set and a
repo that stopped being fetched would leave every task it already has stuck at
`gh_state = open`. Alongside it: `POST /projects/{id}/status`, a
case-insensitive duplicate check in both add paths (`UNIQUE(repo_owner,
repo_name)` is case-*sensitive*, and `Owner/Repo` beside `owner/repo` costs
`resolve_project` its answer and doubles every poll — a check rather than a
unique index, because that migration can *fail* on a database that already holds
the duplicate), and a `resolve_project` that no longer counts archived projects,
so archiving one repo cannot break `POST /issues` for the one that is left.

Two refusals are the load-bearing part and are documented as such. **Mode stays
global**: it gates a dispatcher whose real constraints are server-wide — one
`SCOUT_MAX_CONCURRENT`, one strictly serial build lane, one vm-pool — so a
per-repo `play` could not run while another repo's build held the lane, and a
new store test (`one_repos_running_build_holds_the_lane_against_every_other_repo`)
pins that argument in code; what is honestly per-repo is the *subtraction*.
**There is no delete**: `decisions` is append-only and keyed to a project's
tasks and `tasks.project_id` is `ON DELETE CASCADE`, so archive *is* the
removal, and archived projects are still returned by `GET /projects` and sorted
last in the view rather than dropped — a repo you cannot select is a repo you
cannot un-archive. Both project writes are **human-only and not charter-gated**,
on the `build-now` precedent: they decide what the pipeline is pointed at rather
than doing a unit of work inside it.

On the app side, `projects.rs` (new) holds every switcher decision as a pure
function over lists the server already returned — the filter, the title-bar
label, the archived-last ordering, whether rows should name their repo, the
status notes and transitions, and where a new issue would be filed — all unit
tested. The title bar gains a popover repo switcher (a popover rather than a
menu-bar menu, whose `set_menus` leaks a boxed action per item on every rebuild,
and the item list here *is* the project list), the Tasks list filters to the
selected repo with its done-count footer computed after the filter, and rows in
Tasks and Queue name their repo only when the rows *on screen* disagree about
it, so a single-repo window is unchanged. The Queue is deliberately **not**
filtered: both its reorder endpoints are bulk replaces over the global ordering,
so a narrowed list would still rewrite the ranks of repos it is not showing. A
new Add Repo window (⇧⌘N, and in the File menu) files through `POST /projects`
and selects the new repo by slug once a snapshot holds it, since this client
applies snapshots rather than responses. The issue composer now states the repo
it will file into, refuses to send when several are in view and none is
selected, and carries the `project_id` verbatim so the agent copies a value
rather than re-deriving one — the server already refused to guess, and the app
has stopped asking an agent to.

**Verification: PASSED.** `make test` — 642 passed, 0 failed (6 leaky: the
documented scout-timeout and cancel ones); `cargo clippy --workspace
--all-targets` clean; `cargo fmt --all --check` clean; `make app-check` clean;
`make app-test` 166 passed with its own `cargo fmt --check` clean. The GUI was
compiled and unit-tested but never run — per CLAUDE.md that boundary is real,
so whether the popover lands under the right edge of the title-bar label and
whether the disabled "File issue" button looks disabled are still `make app` on
a Mac. Everything decidable without a pixel was made a pure function in
`projects.rs` and tested there. Note also that the `row_menu.rs` compile fix the
spec carried along was already at HEAD and has been dropped.
