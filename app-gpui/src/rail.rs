//! The left rail: All Tasks, the task tree, Awaiting Feedback, and the rail
//! composer.
//!
//! **The tree is the queue, and its visual order is the priority.** The old
//! Queue section's two facts survive the redesign intact:
//!
//! - A drag writes `tasks.manual_rank` via `POST /queue/reorder`, which is a
//!   **bulk replace** — so every drop posts a complete statement of the
//!   order, computed from the *server's* list order with one row moved,
//!   never from the display order.
//! - The tree **is** scoped by the title bar's repo switcher (unlike the old
//!   Queue section), which is safe only because of the rule above: the
//!   payload base is always the full cross-repo server order, so a drag in a
//!   scoped view moves one row relative to the global ordering and rewrites
//!   nothing it cannot see. What a scoped view *shows* is partial; what it
//!   posts never is.
//!
//! Rows whose place is spent — a Scout or Builder running, a PR open — are
//! neither drag sources nor drop targets, and their glyph says why. Awaiting
//! merge matters most: it shares `manual_rank` with the draggable rows, so a
//! drop there would rewrite the rank of work whose place a pull request
//! already decided.
//!
//! **Awaiting Feedback is deliberately cross-project** — attention must not
//! hide behind the switcher — so its rows carry a project prefix instead.
//! It is the review queue (`spec_queue` pending entries, in rank order);
//! ranking it by drag is a fast-follow, not milestone-2 work.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, SharedString};
use gpuikit::elements::context_menu::context_menu;
use gpuikit::elements::input::text_area;
use gpuikit::elements::kbd::kbd;
use gpuikit::elements::loading_indicator::loading_indicator;
use gpuikit::elements::tooltip::tooltip;
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{Project, SpecQueueItem, SpecQueueStatus, Task, TaskId, TaskState};

use crate::components::{move_to, sidebar, sortable, task_state_color, Sidebar, SidebarSide};
use crate::nav::MiddleView;
use crate::projects::{self, ProjectFilter};
use crate::state::is_picked_up;
use crate::time;
use crate::workspace::Workspace;

/// What a tree row's position means for a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RailReorder {
    /// Draggable: writes `tasks.manual_rank`. `group` keeps Up next and
    /// Ready to build apart — they share the rank, but they are different
    /// lines to be standing in, so a row only lands among its own kind.
    TaskQueue { group: &'static str },
    /// Not reorderable, and the reason why — shown on the row's glyph.
    Fixed(&'static str),
}

const UP_NEXT: &str = "Up next";
const READY_TO_BUILD: &str = "Ready to build";

/// Band order and drag verdict for one state. The tree flattens the old
/// Queue section's bands into one list without headers; attention order
/// survives as the sort. `None` is a task the tree does not show — backlog
/// and history live behind All Tasks, and review lives in Awaiting Feedback.
fn band(state: TaskState) -> Option<(u8, RailReorder)> {
    match state {
        TaskState::Scouting => Some((
            0,
            RailReorder::Fixed("A Scout is running — already dispatched, so its place is spent."),
        )),
        TaskState::Building => Some((
            1,
            RailReorder::Fixed("A Builder is running — already dispatched, so its place is spent."),
        )),
        TaskState::AwaitingMerge => Some((
            2,
            RailReorder::Fixed("Its pull request is open — the merge decides what happens next."),
        )),
        TaskState::Queued => Some((3, RailReorder::TaskQueue { group: UP_NEXT })),
        TaskState::ReadyToBuild => Some((
            4,
            RailReorder::TaskQueue {
                group: READY_TO_BUILD,
            },
        )),
        TaskState::InReview | TaskState::Backlog | TaskState::Done | TaskState::Rejected => None,
    }
}

/// One tree row, as the projection leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RailRow {
    pub task_id: TaskId,
    pub number: u64,
    pub title: String,
    pub state: TaskState,
    pub reorder: RailReorder,
}

/// The tree's rows: picked-up work in attention order, scoped to the
/// window's repo filter. The sort is stable, so within a band the server's
/// own order — which is rank order — survives.
pub(crate) fn tree_rows(tasks: &[Task], filter: &ProjectFilter) -> Vec<RailRow> {
    let mut rows: Vec<(u8, RailRow)> = tasks
        .iter()
        .filter(|task| filter.admits(&task.project_id))
        .filter_map(|task| {
            band(task.state).map(|(band, reorder)| {
                (
                    band,
                    RailRow {
                        task_id: task.id.clone(),
                        number: task.gh_issue_number,
                        title: task.title.clone(),
                        state: task.state,
                        reorder,
                    },
                )
            })
        })
        .collect();
    rows.sort_by_key(|(band, _)| *band);
    rows.into_iter().map(|(_, row)| row).collect()
}

/// One Awaiting Feedback row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackRow {
    pub task_id: TaskId,
    pub number: u64,
    pub title: String,
    /// The repo, named on every row: this section is cross-project by
    /// design, so the prefix is what keeps a row legible with the switcher
    /// pointed elsewhere.
    pub project: Option<String>,
}

/// The review queue, joined to its tasks: pending entries in rank order —
/// the ordering the section reads — with any in-review task no pending entry
/// covers appended rather than dropped, because vanishing from the one
/// attention surface is a worse answer than an unranked row.
pub(crate) fn feedback_rows(
    tasks: &[Task],
    spec_queue: &[SpecQueueItem],
    projects: &[Project],
) -> Vec<FeedbackRow> {
    let mut rows: Vec<FeedbackRow> = Vec::new();
    let push = |task: &Task, rows: &mut Vec<FeedbackRow>| {
        rows.push(FeedbackRow {
            task_id: task.id.clone(),
            number: task.gh_issue_number,
            title: task.title.clone(),
            project: projects::row_label(projects, &task.project_id),
        });
    };
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
        push(task, &mut rows);
    }
    for task in tasks
        .iter()
        .filter(|task| task.state == TaskState::InReview)
    {
        if rows.iter().all(|row| row.task_id != task.id) {
            push(task, &mut rows);
        }
    }
    rows
}

/// The base a drop is computed from: every picked-up task, in the server's
/// order — the *global* ordering, never the scoped or displayed one. See the
/// module comment for why this is what makes a scoped drag safe.
fn task_queue_base(tasks: &[Task]) -> Vec<TaskId> {
    tasks
        .iter()
        .filter(|task| is_picked_up(task.state))
        .map(|task| task.id.clone())
        .collect()
}

/// A tree row in flight. gpui matches drags by `TypeId`, and `group` keeps
/// the two draggable strata apart within the type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RailDrag {
    task_id: TaskId,
    group: &'static str,
}

impl Workspace {
    pub(crate) fn render_left_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The rail's upper level: the repo list. Same shell (header, resize,
        // banner), different body — a drill-down, not a second sidebar.
        if self.rail_shows_repos {
            return self.render_repo_level(cx);
        }
        let theme = cx.theme().clone();
        let (text, text_muted, selected_bg, hover_bg) = (
            theme.fg(),
            theme.fg_muted(),
            theme.surface_tertiary(),
            theme.surface_secondary(),
        );

        // Read before the app-state borrow: a run in flight outranks
        // everything the server could tell us, because it is the reason the
        // server stopped telling us anything.
        let running_op = {
            let control = self.server_control.read(cx);
            control
                .run
                .as_ref()
                .filter(|run| run.is_running())
                .map(|run| (run.op, run.started_at))
        };

        // The pipeline's one sentence, read before the app-state borrow: it
        // joins `AppState` and `ServerControl`, and it is what an empty tree
        // says about itself.
        let explanation = self.explanation(cx);

        // Owned projections first — the rows need `cx` for listeners after
        // the state borrow ends.
        let (tree, feedback, banner, queue_notice) = {
            let state = self.app_state.read(cx);
            let tree = tree_rows(&state.tasks, &self.project_filter);
            let feedback = feedback_rows(&state.tasks, &state.spec_queue, &state.projects);
            let queue_notice = state.queue_notice.clone();

            // A restart in flight outranks both, and is checked first rather
            // than last: it takes the app's own event stream down, and
            // reporting that drop as a transport error would be the app
            // blaming the server for doing what it was asked. A stale build
            // is usually *why* someone hit restart, so this has to sit above
            // the build warning too.
            //
            // The build warning in turn outranks the error: when this app is
            // older than the server supports, whatever failed underneath is
            // the symptom and "your app is old" is the cause.
            let banner = if let Some((op, started_at)) = running_op {
                Some((
                    format!("{}… {}", op.label(), time::elapsed(started_at)),
                    false,
                ))
            } else if let Some(warning) = &state.build_warning {
                Some((warning.clone(), true))
            } else if let Some(error) = &state.error {
                Some((error.clone(), true))
            } else if state.loaded && !state.connected {
                Some(("Reconnecting to the tasks server…".to_string(), false))
            } else {
                None
            };
            (tree, feedback, banner, queue_notice)
        };

        let all_selected = matches!(self.nav.current(), MiddleView::AllTasks);

        sidebar(SidebarSide::Left, self.left_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Left);
                            cx.notify();
                        });
                    }
                }
            })
            // The window chrome lives here now — traffic lights, switcher,
            // pipeline controls. No bar spans the window in the v3 design.
            .child(self.render_rail_header(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .pt(px(4.))
                    // The catalog — the one place backlog lives, and where
                    // queueing happens. A static item above the tree, per the
                    // v3 design.
                    .child(
                        div()
                            .id("nav-all-tasks")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .mx(px(6.))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .when(!all_selected, |el| el.hover(move |el| el.bg(hover_bg)))
                            .when(all_selected, |el| el.bg(selected_bg))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.navigate(MiddleView::AllTasks, cx);
                            }))
                            .child(
                                Icons::list_bullet()
                                    .flex_none()
                                    .size(px(14.))
                                    .text_color(if all_selected { text } else { text_muted }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if all_selected { text } else { text_muted })
                                    .child("All Tasks"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .pl(px(16.))
                            .pr(px(10.))
                            .pt(px(14.))
                            .pb(px(4.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(text_muted)
                                    .child("Tasks"),
                            )
                            // The section's +, per the design — the same
                            // capture surface ⌘N opens.
                            .child(
                                div()
                                    .id("rail-new-task")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(18.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .tooltip(tooltip("New Issue (⌘N)"))
                                    .hover({
                                        let hover_bg = theme.surface_secondary();
                                        move |el| el.bg(hover_bg)
                                    })
                                    .on_click(|_event, window, cx| {
                                        window.dispatch_action(
                                            Box::new(crate::workspace::NewIssue),
                                            cx,
                                        );
                                    })
                                    .child(Icons::plus().size(px(12.)).text_color(text_muted)),
                            ),
                    )
                    // A correction the server made to the last drag, where
                    // the drag happened. Not the banner: the POST succeeded,
                    // and this is about the order, not the connection.
                    .children(queue_notice.map(|notice| {
                        div()
                            .mx(px(6.))
                            .mb(px(4.))
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(px(5.))
                            .bg(theme.surface_secondary())
                            .text_xs()
                            .text_color(theme.fg())
                            .child(notice)
                    }))
                    // A rail that *has* rows and is not moving says why, in
                    // one standing line. Mutually exclusive with the empty
                    // tree's block below, and only because both read a
                    // repo-scoped pipeline: every standing situation needs
                    // something queued, and within a filter an empty tree
                    // means nothing is.
                    .when(!tree.is_empty() && explanation.is_standing(), |el| {
                        el.child(self.render_explanation(&explanation, "rail-standing", true, cx))
                    })
                    // The tree — the queue itself, drag to rank.
                    .child(
                        div()
                            .id("task-tree-scroll")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .map(|el| {
                                if tree.is_empty() {
                                    el.child(self.render_explanation(
                                        &explanation,
                                        "rail-empty",
                                        false,
                                        cx,
                                    ))
                                } else {
                                    el.children(
                                        tree.into_iter().map(|row| self.render_tree_row(row, cx)),
                                    )
                                }
                            }),
                    )
                    // Awaiting Feedback: cross-project attention, under a
                    // dashed rule per the design.
                    .when(!feedback.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_col()
                                .border_t_1()
                                .border_dashed()
                                .border_color(theme.border_subtle())
                                .child(
                                    div()
                                        .px(px(16.))
                                        .pt(px(10.))
                                        .pb(px(4.))
                                        .text_xs()
                                        .text_color(text_muted)
                                        .child("Awaiting Feedback"),
                                )
                                .child(
                                    div()
                                        .id("feedback-scroll")
                                        .flex()
                                        .flex_col()
                                        .max_h(px(220.))
                                        .overflow_y_scroll()
                                        .pb(px(4.))
                                        .children(
                                            feedback
                                                .into_iter()
                                                .map(|row| self.render_feedback_row(row, cx)),
                                        ),
                                ),
                        )
                    }),
            )
            .when_some(banner, |el, (message, is_error)| {
                el.child(
                    div()
                        .m(px(6.))
                        .p(px(8.))
                        .rounded(px(5.))
                        .bg(theme.surface_secondary())
                        .text_xs()
                        .text_color(if is_error {
                            gpui::hsla(30. / 360., 0.9, 0.6, 1.)
                        } else {
                            theme.fg_muted()
                        })
                        .child(message),
                )
            })
            .child(self.render_rail_composer(cx))
    }

    /// The rail's repo level: every added repo, the All-repos scope, and the
    /// way to add one — what the switcher popover used to hold, as a level
    /// of the rail instead. Choosing anything goes back down to its tasks.
    fn render_repo_level(&self, cx: &mut Context<Self>) -> Sidebar {
        let theme = cx.theme().clone();
        let (text, text_muted, selected_bg, hover_bg) = (
            theme.fg(),
            theme.fg_muted(),
            theme.surface_tertiary(),
            theme.surface_secondary(),
        );

        // Owned rows first, as everywhere: listeners need `cx` after the
        // borrow ends.
        let (rows, several) = {
            let state = self.app_state.read(cx);
            let rows: Vec<_> = projects::switcher_order(&state.projects)
                .into_iter()
                .map(|project| {
                    (
                        project.id.clone(),
                        project.repo_owner.clone(),
                        project.repo_name.clone(),
                        projects::status_note(project.status),
                        projects::status_actions(project.status),
                    )
                })
                .collect();
            (rows, state.projects.len() > 1)
        };
        let filter = self.project_filter.clone();

        let mut list = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .pt(px(4.))
            .id("repo-level-scroll")
            .overflow_y_scroll();

        // Only offered when there is something to be "all" of. With one
        // repo configured the window is already showing all of it.
        if several {
            let selected = filter.selected().is_none();
            list = list.child(
                div()
                    .id("repo-all")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
                    .when(selected, |el| el.bg(selected_bg))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.select_project(ProjectFilter::All, window, cx);
                    }))
                    .child(div().text_sm().text_color(text).child("All repos")),
            );
        }

        for (id, owner, name, note, actions) in rows {
            let selected = filter.selected() == Some(&id);
            let mut row = div()
                .id(SharedString::from(format!("repo-{id}")))
                .flex()
                .flex_col()
                .gap(px(1.))
                .mx(px(6.))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(5.))
                .cursor_pointer()
                .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
                .when(selected, |el| el.bg(selected_bg))
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _event, window, cx| {
                        this.select_project(ProjectFilter::One(id.clone()), window, cx);
                    }
                }))
                // The header's two tones, in the list too.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .text_sm()
                        .child(div().text_color(text_muted).child(owner))
                        .child(div().text_color(text).child(name)),
                );
            // Only a repo that is subtracting something carries a note; the
            // ordinary case earns no badge.
            if let Some(note) = note {
                row = row.child(div().text_xs().text_color(text_muted).child(note));
            }
            // The status verbs sit under the repo they act on: there are at
            // most two, and a repo's pipeline stopping is not a thing to
            // bury.
            if !actions.is_empty() {
                row = row.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .pt(px(2.))
                        .text_xs()
                        .children(actions.into_iter().map(|action| {
                            let id = id.clone();
                            div()
                                .id(SharedString::from(format!(
                                    "repo-{id}-{}",
                                    action.status.as_str()
                                )))
                                .text_color(text_muted)
                                .cursor_pointer()
                                .tooltip(tooltip(action.note))
                                .hover(move |el| el.text_color(text))
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.set_project_status(id.clone(), action.status, cx);
                                }))
                                .child(action.label)
                        })),
                );
            }
            list = list.child(row);
        }

        sidebar(SidebarSide::Left, self.left_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Left);
                            cx.notify();
                        });
                    }
                }
            })
            .child(self.render_rail_header(cx))
            .child(list)
            .child(
                div()
                    .flex_none()
                    .m(px(6.))
                    .pt(px(4.))
                    .border_t_1()
                    .border_color(theme.border_subtle())
                    .child(
                        div()
                            .id("repo-add")
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .text_sm()
                            .text_color(text_muted)
                            .hover(move |el| el.bg(hover_bg))
                            .on_click(|_event, window, cx| {
                                window.dispatch_action(Box::new(crate::workspace::AddRepo), cx);
                            })
                            .child("Add repo…"),
                    ),
            )
    }

    /// One tree row: title, number, state glyph; a drag source and target
    /// for its own stratum when its place is still the list's to give.
    fn render_tree_row(&self, row: RailRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let RailRow {
            task_id,
            number,
            title,
            state,
            reorder,
        } = row;
        let selected = self.selected_task.as_ref() == Some(&task_id);
        let hover_bg = theme.surface_secondary();
        let preview = format!("#{number} {title}");

        // Work in motion wears a live indicator; everything else a dot in
        // the state's colour. A fixed row's glyph carries the reason its
        // place is spent.
        let glyph: AnyElement = match state {
            TaskState::Scouting | TaskState::Building => loading_indicator()
                .dash()
                .xsmall()
                .color(theme.accent())
                .into_any_element(),
            state => div()
                .flex_none()
                .size(px(7.))
                .rounded_full()
                .bg(task_state_color(state))
                .into_any_element(),
        };
        let glyph: AnyElement = match reorder {
            RailReorder::Fixed(reason) => div()
                .id(SharedString::from(format!("tree-glyph-{task_id}")))
                .flex_none()
                .tooltip(tooltip(reason))
                .child(glyph)
                .into_any_element(),
            RailReorder::TaskQueue { .. } => glyph,
        };

        let base = div()
            // Keyed by task, never by index: a reorder makes index N a
            // different task before and after the drop, and gpui treats a
            // repeated id across frames as the same node.
            .id(SharedString::from(format!("tree-{task_id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .mx(px(6.))
            .px(px(10.))
            .py(px(4.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
            .when(selected, |el| el.bg(theme.surface_tertiary()))
            .on_click(cx.listener({
                let task_id = task_id.clone();
                move |this, _event, _window, cx| {
                    this.select_task(task_id.clone(), cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .truncate()
                    .text_sm()
                    .text_color(theme.fg())
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(format!("#{number}")),
            )
            .child(div().flex_none().child(glyph));

        // A row is a drag source and a drop target for its own stratum, and
        // for nothing else. `accepts` also refuses the row itself, so the
        // row under the pointer does not light up as its own target.
        let row_element = match reorder {
            RailReorder::TaskQueue { group } => sortable(
                base,
                RailDrag {
                    task_id: task_id.clone(),
                    group,
                },
                preview,
                {
                    let task_id = task_id.clone();
                    move |drag: &RailDrag| drag.group == group && drag.task_id != task_id
                },
                cx.listener({
                    let target = task_id.clone();
                    move |this, drag: &RailDrag, _window, cx| {
                        this.reorder_tree(&drag.task_id, &target, cx);
                    }
                }),
            ),
            RailReorder::Fixed(_) => base,
        };

        context_menu(
            SharedString::from(format!("tree-menu-{task_id}")),
            row_element,
        )
        .menu(Workspace::row_menu(task_id, cx))
        .into_any_element()
    }

    /// One Awaiting Feedback row: `Project › title`, click to open the task
    /// (its Brief is the thing awaiting the verdict).
    fn render_feedback_row(&self, row: FeedbackRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let FeedbackRow {
            task_id,
            number,
            title,
            project,
        } = row;
        let selected = self.selected_task.as_ref() == Some(&task_id);
        let hover_bg = theme.surface_secondary();

        let base = div()
            .id(SharedString::from(format!("feedback-{task_id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .mx(px(6.))
            .px(px(10.))
            .py(px(4.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
            .when(selected, |el| el.bg(theme.surface_tertiary()))
            .on_click(cx.listener({
                let task_id = task_id.clone();
                move |this, _event, _window, cx| {
                    // Straight to the Brief: the spec is what's awaiting
                    // the verdict this section exists to surface.
                    this.open_brief(task_id.clone(), cx);
                }
            }))
            .children(project.map(|project| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(format!("{project} ›"))
            }))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .truncate()
                    .text_sm()
                    .text_color(theme.fg())
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(format!("#{number}")),
            );

        context_menu(SharedString::from(format!("feedback-menu-{task_id}")), base)
            .menu(Workspace::row_menu(task_id, cx))
            .into_any_element()
    }

    /// The rail composer — a second door into the ⌘N flow, not a new path.
    /// The orchestrator titles and files the issue; this box only drafts.
    fn render_rail_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let draft = self.rail_input.read(cx).content();
        let has_text = !draft.trim().is_empty();
        let target = {
            let state = self.app_state.read(cx);
            projects::issue_target(&state.projects, &self.project_filter)
        };
        let can_send = has_text && target.can_file();
        let lines = draft.lines().count().clamp(2, 6);
        let composer_height = px(22. * lines as f32 + 16.);

        // One bordered box holding the draft and its Send, the way the
        // design draws it; the target sentence sits under the box, small,
        // because which repo this files into must never be a guess.
        div()
            .flex_none()
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.border_secondary())
                    .bg(theme.bg())
                    .child(
                        div()
                            .h(composer_height)
                            .text_sm()
                            .child(text_area(&self.rail_input, cx).size_full()),
                    )
                    .child(
                        div().flex().flex_row().items_center().justify_end().child(
                            div()
                                .id("rail-send")
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.))
                                .px(px(8.))
                                .py(px(3.))
                                .rounded(px(5.))
                                .text_xs()
                                .map(|el| {
                                    if can_send {
                                        let hover_bg = theme.surface_secondary();
                                        el.cursor_pointer()
                                            .text_color(theme.fg())
                                            .hover(move |el| el.bg(hover_bg))
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.submit_rail_composer(cx);
                                            }))
                                    } else {
                                        el.text_color(theme.fg_muted()).opacity(0.5)
                                    }
                                })
                                .child("Send")
                                .child(kbd("⌘↩")),
                        ),
                    ),
            )
            // Only when it has something to warn about: the ordinary
            // one-repo case needs no caption under the box.
            .when(!target.can_file() || has_text, |el| {
                el.child(
                    div()
                        .px(px(2.))
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .overflow_hidden()
                        .truncate()
                        .child(target.sentence()),
                )
            })
    }

    /// Submit the rail draft: hand it to the orchestrator with the window's
    /// repo pinned, exactly as the ⌘N window does. Refuses (to the banner)
    /// while the target is ambiguous — the guess this flow exists to stop
    /// asking an agent to make.
    pub(crate) fn submit_rail_composer(&mut self, cx: &mut Context<Self>) {
        let draft = self.rail_input.read(cx).content().trim().to_string();
        if draft.is_empty() {
            return;
        }
        let target = {
            let state = self.app_state.read(cx);
            projects::issue_target(&state.projects, &self.project_filter)
        };
        let Some(message) = projects::issue_prompt(&target, &draft) else {
            self.report("choose a repo in the title bar to file this into", cx);
            return;
        };
        self.rail_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.ask_orchestrator(message, cx);
    }

    /// A tree row was dropped on another: post the whole order, computed
    /// from the server's list *now* rather than from the snapshot the drag
    /// started in.
    fn reorder_tree(&mut self, moved: &TaskId, target: &TaskId, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            let base = task_queue_base(&state.tasks);
            if let Some(order) = move_to(&base, moved, target) {
                state.reorder_queue(order, cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{GhState, ProjectId, SpecId, SpecQueueEntry};

    use super::*;

    fn task(number: u64, state: TaskState) -> Task {
        task_in(number, state, "proj-1")
    }

    fn task_in(number: u64, state: TaskState, project: &str) -> Task {
        Task {
            id: TaskId::from_raw(format!("task-{number}")),
            project_id: ProjectId::from_raw(project),
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
            scout_directions: None,
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

    fn numbers(rows: &[RailRow]) -> Vec<u64> {
        rows.iter().map(|row| row.number).collect()
    }

    /// The tree holds pipeline work in attention order, flattened; within a
    /// band the server's order (rank order) survives. Backlog, review and
    /// history are elsewhere.
    #[test]
    fn the_tree_is_attention_ordered_and_review_free() {
        let tasks = [
            task(1, TaskState::Queued),
            task(2, TaskState::Scouting),
            task(3, TaskState::Queued),
            task(4, TaskState::InReview),
            task(5, TaskState::Backlog),
            task(6, TaskState::ReadyToBuild),
            task(7, TaskState::AwaitingMerge),
            task(8, TaskState::Building),
            task(9, TaskState::Done),
        ];
        let rows = tree_rows(&tasks, &ProjectFilter::All);
        assert_eq!(numbers(&rows), [2, 8, 7, 1, 3, 6]);
    }

    /// The switcher scopes what the tree shows…
    #[test]
    fn the_tree_is_scoped_by_the_repo_filter() {
        let tasks = [
            task_in(1, TaskState::Queued, "proj-1"),
            task_in(2, TaskState::Queued, "proj-2"),
        ];
        let rows = tree_rows(&tasks, &ProjectFilter::One(ProjectId::from_raw("proj-2")));
        assert_eq!(numbers(&rows), [2]);
    }

    /// …but never what a drop posts: the base is every picked-up task in the
    /// server's cross-repo order, so a scoped drag cannot rewrite ranks it
    /// is not showing into a display order.
    #[test]
    fn the_reorder_base_is_the_global_server_order() {
        let tasks = [
            task_in(1, TaskState::Queued, "proj-1"),
            task_in(2, TaskState::Backlog, "proj-1"),
            task_in(3, TaskState::Queued, "proj-2"),
            task_in(4, TaskState::InReview, "proj-2"),
            task_in(5, TaskState::Done, "proj-1"),
        ];
        let base = task_queue_base(&tasks);
        let ids: Vec<_> = base.iter().map(|id| id.to_string()).collect();
        assert_eq!(ids, ["task-1", "task-3", "task-4"]);
    }

    /// Dispatched work is fixed; the two waiting strata are draggable and
    /// kept apart, because they share `manual_rank` but are different lines.
    #[test]
    fn only_waiting_work_is_draggable_and_strata_stay_apart() {
        let verdict = |state: TaskState| band(state).unwrap().1;
        assert!(matches!(
            verdict(TaskState::Scouting),
            RailReorder::Fixed(_)
        ));
        assert!(matches!(
            verdict(TaskState::Building),
            RailReorder::Fixed(_)
        ));
        // Not merely undispatchable: it shares `manual_rank` with the two
        // draggable strata, so a drop here would rewrite ranks a pull
        // request owns.
        assert!(matches!(
            verdict(TaskState::AwaitingMerge),
            RailReorder::Fixed(_)
        ));
        let (RailReorder::TaskQueue { group: up_next }, RailReorder::TaskQueue { group: ready }) =
            (verdict(TaskState::Queued), verdict(TaskState::ReadyToBuild))
        else {
            panic!("waiting work must be draggable");
        };
        assert_ne!(up_next, ready);
    }

    /// Feedback follows the spec queue's order — the ordering it will one
    /// day write — and keeps uncovered in-review tasks visible, unranked.
    #[test]
    fn feedback_follows_spec_queue_order_and_keeps_uncovered_tasks() {
        let tasks = [
            task(1, TaskState::InReview),
            task(2, TaskState::InReview),
            task(3, TaskState::InReview),
        ];
        let queue = [
            entry(2, SpecQueueStatus::PendingReview),
            entry(1, SpecQueueStatus::PendingReview),
            // Approved: ranks nothing here.
            entry(3, SpecQueueStatus::Approved),
        ];
        let rows = feedback_rows(&tasks, &queue, &[]);
        let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
        assert_eq!(numbers, [2, 1, 3]);
    }

    /// A pending entry whose task moved on does not resurrect it, and a
    /// re-scout's second entry does not double a row.
    #[test]
    fn feedback_shows_only_live_review_and_never_doubles() {
        let tasks = [
            task(1, TaskState::ReadyToBuild),
            task(2, TaskState::InReview),
        ];
        let queue = [
            entry(1, SpecQueueStatus::PendingReview),
            entry(2, SpecQueueStatus::PendingReview),
            entry(2, SpecQueueStatus::PendingReview),
        ];
        let rows = feedback_rows(&tasks, &queue, &[]);
        let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
        assert_eq!(numbers, [2]);
    }
}
