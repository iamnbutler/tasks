//! The Queue section: picked-up work as a column-aligned table of draggable
//! rows, in attention order — Needs you / Running / Building / Awaiting merge /
//! Up next / Ready to build.
//!
//! **The order of the rows is the priority, and a drag writes it.** Two
//! orderings meet in this one view, and each band is sorted by, and writes,
//! exactly one of them:
//!
//! - **Needs you** is the review queue: `spec_queue.rank`, via
//!   `POST /spec-queue/reorder`.
//! - **Up next** and **Ready to build** are the task queue:
//!   `tasks.manual_rank`, via `POST /queue/reorder` — the ordering
//!   `next_dispatchable` walks, so a drag there decides what a Scout picks up
//!   next.
//! - **Running**, **Building** and **Awaiting merge** are out of the list's
//!   hands. They wear a lock, say why in a tooltip, and are not drop targets.
//!   Awaiting merge matters most here: it shares `tasks.manual_rank` with Up
//!   next and Ready to build, so making it draggable would let a drop rewrite
//!   the rank of work whose place a pull request already decided.
//!
//! A band must be *sorted by* the ordering it writes: sorting Needs you by
//! `manual_rank` while posting spec ranks would land a drag that succeeded and
//! moved nothing on screen.
//!
//! Both endpoints are a **bulk replace**, not a patch — they unrank
//! everything and then assign 1..N over the ids they are given. So every drop
//! posts a complete statement of the order, and the base it is computed from
//! is the *server's* list order with one row moved, never the display order:
//! concatenating the bands top to bottom would rewrite Scouting and InReview
//! ranks to match the visual grouping, turning a local drag into a global
//! reorder. See [`AppState::reorder_queue`] for what happens after the POST.
//!
//! **This section is deliberately not filtered by the title bar's repo
//! switcher**, and that follows from the same fact. Both reorder endpoints
//! unrank everything and assign 1..N over the ids they are given, and each
//! drop's payload is computed from the *server's* list order — so a narrowed
//! list would still rewrite the ranks of repos it is not showing, and a row
//! dropped "second" would land second among rows the human cannot see. The
//! Queue also *is* the global ordering, which is the same fact that keeps mode
//! global. Rows name their repo instead, whenever the section holds more than
//! one.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, SharedString};
use gpuikit::elements::context_menu::context_menu;
use gpuikit::elements::tooltip::tooltip;
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{
    BuildStatus, ProjectId, SpecId, SpecQueueItem, SpecQueueStatus, Task, TaskId, TaskState,
};

use crate::components::{move_to, sortable, status_badge, task_state_color, title_case};
use crate::projects;
use crate::state::{is_picked_up, AppState};
use crate::time;
use crate::workspace::Workspace;

/// Which ordering a band is sorted by, and therefore which one it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reorder {
    /// `tasks.manual_rank`.
    TaskQueue,
    /// `spec_queue.rank`.
    SpecQueue,
    /// Not reorderable, and the reason why — shown on the row's lock.
    Fixed(&'static str),
}

/// One band of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Group {
    label: &'static str,
    state: TaskState,
    reorder: Reorder,
}

/// The bands, in attention order. The one place a band's label, its state and
/// the ordering it writes are stated together.
const GROUPS: [Group; 6] = [
    Group {
        label: "Needs you",
        state: TaskState::InReview,
        reorder: Reorder::SpecQueue,
    },
    Group {
        label: "Running",
        state: TaskState::Scouting,
        reorder: Reorder::Fixed("A Scout is running — already dispatched, so its place is spent."),
    },
    Group {
        label: "Building",
        state: TaskState::Building,
        reorder: Reorder::Fixed(
            "A Builder is running — already dispatched, so its place is spent.",
        ),
    },
    // Between Building and Up next: the work is written but not shipped, and
    // the poller is still driving it. A pull request decides its place, so the
    // list does not.
    Group {
        label: "Awaiting merge",
        state: TaskState::AwaitingMerge,
        reorder: Reorder::Fixed("Its pull request is open — the merge decides what happens next."),
    },
    Group {
        label: "Up next",
        state: TaskState::Queued,
        reorder: Reorder::TaskQueue,
    },
    Group {
        label: "Ready to build",
        state: TaskState::ReadyToBuild,
        reorder: Reorder::TaskQueue,
    },
];

/// One row, as the projection leaves it. What it is, not what it looks like:
/// the accessory (a complexity word, a live clock) needs specs, sessions and
/// builds, and is attached during the render pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BandRow {
    task_id: TaskId,
    project_id: ProjectId,
    number: u64,
    title: String,
    /// The spec whose queue entry ranks this row. Only the review band has
    /// one, and even there it is optional: a task in review that no pending
    /// entry covers is still shown, and simply cannot be dragged, because
    /// there is nothing to rank.
    spec_id: Option<SpecId>,
}

fn band_row(task: &Task) -> BandRow {
    BandRow {
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        number: task.gh_issue_number,
        title: task.title.clone(),
        spec_id: None,
    }
}

/// The section's rows, banded — the pure projection the table renders.
///
/// Every band but the review one comes from `tasks`, which the server hands
/// back in `manual_rank` order. The review band comes from `spec_queue`, in
/// rank order, because that is the ordering it writes.
fn bands(tasks: &[Task], spec_queue: &[SpecQueueItem]) -> Vec<(Group, Vec<BandRow>)> {
    GROUPS
        .iter()
        .map(|group| {
            let rows = match group.reorder {
                Reorder::SpecQueue => review_band(tasks, spec_queue),
                _ => tasks
                    .iter()
                    .filter(|task| task.state == group.state)
                    .map(band_row)
                    .collect(),
            };
            (*group, rows)
        })
        .collect()
}

/// Needs you: the pending-review entries in rank order, joined to their tasks.
///
/// Anything in review that no pending entry covers is appended rather than
/// dropped — projecting the band from `spec_queue` alone would make such a
/// task vanish from the section entirely, which is a worse answer than an
/// un-draggable row.
fn review_band(tasks: &[Task], spec_queue: &[SpecQueueItem]) -> Vec<BandRow> {
    let mut rows: Vec<BandRow> = Vec::new();
    for item in spec_queue
        .iter()
        .filter(|item| item.entry.status == SpecQueueStatus::PendingReview)
    {
        let Some(task) = tasks.iter().find(|task| task.id == item.task_id) else {
            continue;
        };
        if task.state != TaskState::InReview || rows.iter().any(|row| row.task_id == task.id) {
            continue;
        }
        rows.push(BandRow {
            spec_id: Some(item.entry.spec_id.clone()),
            ..band_row(task)
        });
    }
    for task in tasks
        .iter()
        .filter(|task| task.state == TaskState::InReview)
    {
        if rows.iter().all(|row| row.task_id != task.id) {
            rows.push(band_row(task));
        }
    }
    rows
}

/// The base a task-queue drop is computed from: every picked-up task, in the
/// server's order. Not the display order — see the module comment.
fn task_queue_base(tasks: &[Task]) -> Vec<TaskId> {
    tasks
        .iter()
        .filter(|task| is_picked_up(task.state))
        .map(|task| task.id.clone())
        .collect()
}

/// The base a review-queue drop is computed from: *every* queue entry, in the
/// server's order — not just the pending-review ones this band shows, because
/// the endpoint unranks everything it is not given.
fn spec_queue_base(spec_queue: &[SpecQueueItem]) -> Vec<SpecId> {
    spec_queue
        .iter()
        .map(|item| item.entry.spec_id.clone())
        .collect()
}

/// A row of the task queue in flight. gpui matches drags by `TypeId`, so a
/// review row can never land here; `group` keeps Up next and Ready to build
/// apart within the type — they share `manual_rank`, but they are different
/// lines to be standing in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskQueueDrag {
    task_id: TaskId,
    group: &'static str,
}

/// A row of the review queue in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecQueueDrag {
    spec_id: SpecId,
}

/// Column widths, so the header and every row agree without a layout pass.
const HANDLE_W: f32 = 14.;
const ISSUE_W: f32 = 46.;
const STATUS_W: f32 = 92.;
const TRAILING_W: f32 = 58.;

/// A row with the accessory the render pass attached: a complexity word, or a
/// live clock.
struct QueueRow {
    row: BandRow,
    trailing: Option<String>,
    /// The trailing text is a running clock, and is styled as active work
    /// rather than as a note.
    live: bool,
    /// The repo this row belongs to, when the rows on screen disagree about
    /// it. Attached in the render pass, because the answer is about the whole
    /// section rather than about one band.
    repo: Option<String>,
}

/// [`QueueRow`] for a projected row — the one place that needs specs, sessions
/// and builds, which is why it is not part of [`bands`].
fn queue_row(group: &Group, row: BandRow, state: &AppState, name_repos: bool) -> QueueRow {
    let (trailing, live) = match group.state {
        TaskState::InReview | TaskState::ReadyToBuild => (
            state
                .latest_spec(&row.task_id)
                .map(|spec| spec.complexity.as_str().to_string()),
            false,
        ),
        TaskState::Scouting => match state.running_session(&row.task_id) {
            Some(session) => (Some(time::elapsed(session.started_at)), true),
            None => (None, false),
        },
        TaskState::Building => {
            let started = state
                .builds
                .iter()
                .find(|build| build.status == BuildStatus::Running)
                .and_then(|build| build.started_at);
            match started {
                Some(started) => (Some(time::elapsed(started)), true),
                None => (None, false),
            }
        }
        _ => (None, false),
    };
    let repo = name_repos
        .then(|| projects::row_label(&state.projects, &row.project_id))
        .flatten();
    QueueRow {
        row,
        trailing,
        live,
        repo,
    }
}

impl Workspace {
    pub(crate) fn render_queue(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.selected_task.clone();

        // Everything read out of app state up front and owned: the listeners
        // below need `cx` mutably, so no borrow of it may still be alive.
        let (bands, notice, loaded) = {
            let state = self.app_state.read(cx);
            let projected = bands(&state.tasks, &state.spec_queue);
            // Across the whole section, not per band: the Queue *is* the
            // global ordering (see the module comment on why it is not
            // filtered), so a row names its repo whenever the section holds
            // more than one.
            let name_repos = projects::rows_are_ambiguous(
                projected
                    .iter()
                    .flat_map(|(_, rows)| rows.iter().map(|row| &row.project_id)),
            );
            let bands: Vec<(Group, Vec<QueueRow>)> = projected
                .into_iter()
                .map(|(group, rows)| {
                    let rows = rows
                        .into_iter()
                        .map(|row| queue_row(&group, row, state, name_repos))
                        .collect();
                    (group, rows)
                })
                .collect();
            (bands, state.queue_notice.clone(), state.loaded)
        };
        let has_rows = bands.iter().any(|(_, rows)| !rows.is_empty());

        let mut list = div()
            .id("queue-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .py(px(4.));

        // A correction the server made to the last drag, in the one place the
        // drag happened. Not the sidebar banner: the POST succeeded, and this
        // is about the order, not about the connection.
        if let Some(notice) = notice {
            list = list.child(
                div()
                    .mx(px(6.))
                    .mt(px(4.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(5.))
                    .bg(theme.surface_secondary())
                    .text_xs()
                    .text_color(theme.fg())
                    .child(notice),
            );
        }

        if !has_rows {
            // Before the first snapshot there is nothing to say yet — and a
            // column header over no rows is a table claiming to hold none.
            if loaded {
                list = list.child(
                    div()
                        .p(px(16.))
                        .text_sm()
                        .text_color(theme.fg_muted())
                        .child("Nothing queued — pick tasks up from the Tasks list."),
                );
            }
            return list;
        }

        list = list.child(self.queue_header(cx));

        for (group, rows) in bands {
            if rows.is_empty() {
                continue;
            }
            let count = rows.len();
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(16.))
                    .pt(px(12.))
                    .pb(px(4.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(group.label.to_uppercase())
                    .child(div().child("·"))
                    .child(div().child(count.to_string())),
            );
            for row in rows {
                list = list.child(self.render_queue_row(group, row, selected.as_ref(), cx));
            }
        }

        list
    }

    /// The column header. What makes the section read as a table rather than
    /// as five lists that happen to line up.
    fn queue_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let cell = |label: &'static str, width: f32| {
            div()
                .w(px(width))
                .flex_none()
                .text_xs()
                .text_color(theme.fg_muted())
                .child(label)
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .mx(px(6.))
            .px(px(10.))
            .pb(px(6.))
            .border_b_1()
            .border_color(theme.border_subtle())
            .child(div().w(px(HANDLE_W)).flex_none())
            .child(cell("ISSUE", ISSUE_W))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child("TASK"),
            )
            .child(cell("STATUS", STATUS_W))
            .child(div().w(px(TRAILING_W)).flex_none())
    }

    fn render_queue_row(
        &self,
        group: Group,
        item: QueueRow,
        selected: Option<&TaskId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let QueueRow {
            row,
            trailing,
            live,
            repo,
        } = item;
        let theme = cx.theme().clone();
        let task_id = row.task_id.clone();
        let is_selected = selected == Some(&task_id);
        let preview = format!("#{} {}", row.number, row.title);

        // Only the locked handle is stateful. A drag starts from the *row*, so
        // an id'd child sits in front of the thing being dragged in the hitbox
        // order; a locked row is not a drag source, so its tooltip cannot get
        // in the way. Draggable rows say so with the glyph and `cursor_grab`.
        let handle = match group.reorder {
            Reorder::Fixed(reason) => div()
                .id(SharedString::from(format!("queue-lock-{task_id}")))
                .flex_none()
                .tooltip(tooltip(reason))
                .child(
                    Icons::lock_closed()
                        .size(px(HANDLE_W))
                        .text_color(theme.fg_muted()),
                )
                .into_any_element(),
            // In review, but no pending entry ranks it: nothing to drag.
            Reorder::SpecQueue if row.spec_id.is_none() => {
                div().w(px(HANDLE_W)).flex_none().into_any_element()
            }
            _ => div()
                .flex_none()
                .child(
                    Icons::drag_handle_dots_2()
                        .size(px(HANDLE_W))
                        .text_color(theme.fg_muted()),
                )
                .into_any_element(),
        };

        let base = div()
            // Keyed by task, never by index: a reorder makes index N a
            // different task before and after the drop, and gpui treats a
            // repeated id across frames as the same node.
            .id(SharedString::from(format!("queue-{task_id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .mx(px(6.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(!is_selected, |el| {
                let hover_bg = theme.surface_secondary();
                el.hover(move |el| el.bg(hover_bg))
            })
            .when(is_selected, |el| el.bg(theme.surface_tertiary()))
            .on_click(cx.listener({
                let task_id = task_id.clone();
                move |this, _event, _window, cx| {
                    this.select_task(task_id.clone(), cx);
                }
            }))
            .child(handle)
            .child(
                div()
                    .w(px(ISSUE_W))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(format!("#{}", row.number)),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_sm()
                    .text_color(theme.fg())
                    .truncate()
                    .child(row.title.clone()),
            )
            // The Queue is deliberately not filtered by repo — its two reorder
            // endpoints are bulk replaces over the *global* ordering — so a row
            // names its repo instead.
            .children(repo.map(|repo| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(repo)
            }))
            .child(
                div()
                    .w(px(STATUS_W))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .child(status_badge(
                        title_case(group.state.as_str()),
                        task_state_color(group.state),
                    )),
            )
            .child(
                div()
                    .w(px(TRAILING_W))
                    .flex_none()
                    .text_xs()
                    .text_color(if live {
                        theme.accent()
                    } else {
                        theme.fg_muted()
                    })
                    .truncate()
                    .child(trailing.unwrap_or_default()),
            );

        // A row is a drag source and a drop target for the ordering its band
        // writes, and for nothing else. `accepts` also refuses the row itself,
        // so the row under the pointer does not light up as its own target.
        let row_element = match (group.reorder, row.spec_id) {
            (Reorder::TaskQueue, _) => sortable(
                base,
                TaskQueueDrag {
                    task_id: task_id.clone(),
                    group: group.label,
                },
                preview,
                {
                    let task_id = task_id.clone();
                    move |drag: &TaskQueueDrag| drag.group == group.label && drag.task_id != task_id
                },
                cx.listener({
                    let target = task_id.clone();
                    move |this, drag: &TaskQueueDrag, _window, cx| {
                        this.reorder_task_queue(&drag.task_id, &target, cx);
                    }
                }),
            ),
            (Reorder::SpecQueue, Some(spec_id)) => sortable(
                base,
                SpecQueueDrag {
                    spec_id: spec_id.clone(),
                },
                preview,
                {
                    let spec_id = spec_id.clone();
                    move |drag: &SpecQueueDrag| drag.spec_id != spec_id
                },
                cx.listener(move |this, drag: &SpecQueueDrag, _window, cx| {
                    this.reorder_review_queue(&drag.spec_id, &spec_id, cx);
                }),
            ),
            // Dispatched work, and review rows nothing ranks: neither a source
            // nor a target.
            _ => base,
        };

        // Right-click offers what the Tasks list offers, greyed to this row's
        // state. Keyed by task, like the row — and prefixed, because the menu's
        // open state is keyed globally and the Tasks list has one per task too.
        context_menu(
            SharedString::from(format!("queue-row-menu-{task_id}")),
            row_element,
        )
        .menu(Workspace::row_menu(task_id, cx))
        .into_any_element()
    }

    /// A task-queue row was dropped on another: post the whole order, computed
    /// from the server's list *now* rather than from the snapshot the drag
    /// started in.
    fn reorder_task_queue(&mut self, moved: &TaskId, target: &TaskId, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            let base = task_queue_base(&state.tasks);
            if let Some(order) = move_to(&base, moved, target) {
                state.reorder_queue(order, cx);
            }
        });
    }

    /// The same for the review queue, which ranks specs rather than tasks.
    fn reorder_review_queue(&mut self, moved: &SpecId, target: &SpecId, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            let base = spec_queue_base(&state.spec_queue);
            if let Some(order) = move_to(&base, moved, target) {
                state.reorder_spec_queue(order, cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{GhState, ProjectId, SpecQueueEntry};

    use super::*;

    fn task(number: u64, state: TaskState) -> Task {
        Task {
            id: TaskId::from_raw(format!("task-{number}")),
            project_id: ProjectId::from_raw("proj-1"),
            gh_issue_number: number,
            title: format!("issue {number}"),
            body: String::new(),
            labels: Vec::new(),
            gh_state: GhState::Open,
            state,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: Utc.timestamp_opt(0, 0).unwrap(),
            updated_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    fn entry(number: u64, status: SpecQueueStatus) -> SpecQueueItem {
        SpecQueueItem {
            entry: SpecQueueEntry {
                spec_id: SpecId::from_raw(format!("spec-{number}")),
                status,
                rank: None,
                approved_at: None,
                feedback: None,
                blocking_dependencies: Vec::new(),
            },
            task_id: TaskId::from_raw(format!("task-{number}")),
        }
    }

    fn band(bands: &[(Group, Vec<BandRow>)], label: &str) -> Vec<u64> {
        bands
            .iter()
            .find(|(group, _)| group.label == label)
            .map(|(_, rows)| rows.iter().map(|row| row.number).collect())
            .unwrap_or_default()
    }

    /// Every band shows exactly the state it is named for, in the order the
    /// server handed the list over.
    #[test]
    fn each_band_holds_its_own_state_in_the_servers_order() {
        let tasks = [
            task(3, TaskState::Queued),
            task(1, TaskState::Scouting),
            task(2, TaskState::Queued),
            task(4, TaskState::ReadyToBuild),
            task(5, TaskState::Building),
            task(6, TaskState::AwaitingMerge),
        ];
        let bands = bands(&tasks, &[]);
        assert_eq!(band(&bands, "Up next"), [3, 2]);
        assert_eq!(band(&bands, "Running"), [1]);
        assert_eq!(band(&bands, "Ready to build"), [4]);
        assert_eq!(band(&bands, "Building"), [5]);
        assert_eq!(band(&bands, "Awaiting merge"), [6]);
    }

    /// Needs you follows `spec_queue`, which is the ordering it writes — not
    /// the task list's `manual_rank`, which it does not.
    #[test]
    fn the_review_band_follows_the_spec_queues_order() {
        let tasks = [task(1, TaskState::InReview), task(2, TaskState::InReview)];
        let queue = [
            entry(2, SpecQueueStatus::PendingReview),
            entry(1, SpecQueueStatus::PendingReview),
        ];
        let bands = bands(&tasks, &queue);
        assert_eq!(band(&bands, "Needs you"), [2, 1]);
    }

    #[test]
    fn a_review_row_carries_the_spec_that_ranks_it() {
        let tasks = [task(1, TaskState::InReview)];
        let queue = [entry(1, SpecQueueStatus::PendingReview)];
        let bands = bands(&tasks, &queue);
        let (_, rows) = bands
            .iter()
            .find(|(group, _)| group.label == "Needs you")
            .unwrap();
        assert_eq!(rows[0].spec_id, Some(SpecId::from_raw("spec-1")));
    }

    /// A task in review that no pending entry covers is visible and
    /// un-draggable, rather than missing from the section.
    #[test]
    fn a_review_row_with_no_pending_entry_is_kept_without_a_spec() {
        let tasks = [task(1, TaskState::InReview), task(2, TaskState::InReview)];
        let queue = [
            entry(1, SpecQueueStatus::PendingReview),
            // Already approved: it ranks nothing in this band.
            entry(2, SpecQueueStatus::Approved),
        ];
        let bands = bands(&tasks, &queue);
        assert_eq!(band(&bands, "Needs you"), [1, 2]);
        let (_, rows) = bands
            .iter()
            .find(|(group, _)| group.label == "Needs you")
            .unwrap();
        assert_eq!(rows[1].spec_id, None);
    }

    /// Two pending entries for one task (a re-scout) is one row, not two.
    #[test]
    fn a_task_with_two_pending_entries_appears_once() {
        let tasks = [task(1, TaskState::InReview)];
        let queue = [
            entry(1, SpecQueueStatus::PendingReview),
            entry(1, SpecQueueStatus::PendingReview),
        ];
        assert_eq!(band(&bands(&tasks, &queue), "Needs you"), [1]);
    }

    /// A pending entry whose task has moved on does not resurrect it here.
    #[test]
    fn a_pending_entry_whose_task_left_review_is_not_shown() {
        let tasks = [task(1, TaskState::ReadyToBuild)];
        let queue = [entry(1, SpecQueueStatus::PendingReview)];
        assert!(band(&bands(&tasks, &queue), "Needs you").is_empty());
    }

    /// The POST base is the server's list order, not the display order the
    /// bands make: a local drag must not rewrite the ranks of every other
    /// picked-up row to match the visual grouping.
    #[test]
    fn the_task_queue_base_is_every_picked_up_task_in_server_order() {
        let tasks = [
            task(1, TaskState::Queued),
            task(2, TaskState::Backlog),
            task(3, TaskState::Scouting),
            task(4, TaskState::Done),
            task(5, TaskState::InReview),
        ];
        let base = task_queue_base(&tasks);
        let ids: Vec<_> = base.iter().map(|id| id.to_string()).collect();
        assert_eq!(ids, ["task-1", "task-3", "task-5"]);
    }

    /// Every entry, not just the pending-review ones the band shows: the
    /// endpoint unranks whatever it is not given.
    #[test]
    fn the_spec_queue_base_is_every_entry() {
        let queue = [
            entry(1, SpecQueueStatus::PendingReview),
            entry(2, SpecQueueStatus::Approved),
            entry(3, SpecQueueStatus::Rejected),
        ];
        let base = spec_queue_base(&queue);
        let ids: Vec<_> = base.iter().map(|id| id.to_string()).collect();
        assert_eq!(ids, ["spec-1", "spec-2", "spec-3"]);
    }

    /// One band per state, and the two orderings land where they belong:
    /// dispatched work is fixed, the review band writes spec ranks, and the
    /// two waiting bands write `manual_rank`.
    #[test]
    fn every_state_has_one_band_and_the_right_verdict() {
        let states: Vec<_> = GROUPS.iter().map(|group| group.state).collect();
        for group in GROUPS {
            assert_eq!(
                states.iter().filter(|state| **state == group.state).count(),
                1,
                "{} shares its state with another band",
                group.label
            );
            assert!(is_picked_up(group.state), "{}", group.label);
        }
        let verdict = |label: &str| {
            GROUPS
                .iter()
                .find(|group| group.label == label)
                .unwrap()
                .reorder
        };
        assert_eq!(verdict("Needs you"), Reorder::SpecQueue);
        assert_eq!(verdict("Up next"), Reorder::TaskQueue);
        assert_eq!(verdict("Ready to build"), Reorder::TaskQueue);
        assert!(matches!(verdict("Running"), Reorder::Fixed(_)));
        assert!(matches!(verdict("Building"), Reorder::Fixed(_)));
        // Not merely undispatchable: it shares `manual_rank` with the two
        // TaskQueue bands, so a drop here would rewrite ranks a pull request
        // owns.
        assert!(matches!(verdict("Awaiting merge"), Reorder::Fixed(_)));
    }
}
