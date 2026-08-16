# Queue becomes a table of draggable rows whose order *is* the priority

The Queue section is now a column-aligned table (`ISSUE / TASK / STATUS`, plus
a grip and a trailing accessory) whose rows are dragged, and the resulting
order is written straight back to the server as the priority. Two orderings
meet in this one view, and each band is sorted by — and writes — exactly one of
them: **Needs you** is the review queue (`spec_queue.rank`, via
`POST /spec-queue/reorder`) and **Up next** / **Ready to build** are the task
queue (`tasks.manual_rank`, via `POST /queue/reorder`, the ordering
`next_dispatchable` walks, so a drag there decides what a Scout picks up next).
**Running** and **Building** are already dispatched: they wear a lock, say why
in a tooltip, and are not drop targets. A band has to be sorted by the ordering
it writes, or a drag succeeds and moves nothing on screen. Nothing under
`crates/` changed — both endpoints and both `tasks-client` methods already
existed.

The drag primitive lands in `app-gpui/src/components/sortable.rs`, built on
gpui's typed `on_drag` / `drag_over` / `on_drop` and deliberately
self-contained (it imports `gpui` and `gpuikit::theme` and knows nothing about
tasks or ranks), so lifting it into gpuikit later is a file move rather than a
rewrite. `sortable` consults its `accepts` predicate in both the drop handler
and the `drag_over` style, because `can_drop` gates only the drop — a row
filtered by `can_drop` alone still lights up as a target and then refuses it —
and it returns the decorated row so the existing context menu composes onto it
unchanged. The one fact that drives the rest of the design is that both reorder
endpoints are a **bulk replace**, not a patch: they unrank everything and then
assign 1..N over the ids they are given. So every drop posts a complete
statement of the order, computed from the *server's* list order with one row
moved — never from the display order, which would rewrite Scouting and InReview
ranks to match the visual grouping and turn a local drag into a global reorder.
The response is read rather than assumed: a drop computed before a concurrent
`POST /tasks/{id}/queue` omits the newly queued task, which comes back unranked
at the bottom, and `queue_notice` says so in the section instead of letting it
be silent. Rows are keyed by `TaskId`, since after a reorder index N is a
different task before and after the drop. Tests cover the pure functions —
`move_to`, `bands` / `task_queue_base` / `spec_queue_base` / the `GROUPS`
invariants, and `ranked_first` / `lost_queue_places` / `lost_review_places` —
27 new ones, with `cargo fmt --check`, `cargo clippy --all-targets` and the
app's suite (99 passed) clean.

Not done, deliberately: no auto-scroll when a drag reaches the edge of the
scroll container (gpui offers no primitive and the queue is short), no keyboard
reorder (⌥↑/⌥↓ would reuse `move_to` unchanged), and no multi-row drag. One
decision worth a reviewer's attention: the review band now follows
`spec_queue.rank` rather than `manual_rank`, which changes its default order
from pickup order to oldest-spec-first (the endpoint's fallback, since nothing
writes `spec_queue.rank` today). That is the consequence of making a band write
the ordering it displays; the alternative would leave
`POST /spec-queue/reorder` with no UI at all.
