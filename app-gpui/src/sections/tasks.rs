//! The Tasks section: every task in the working set, Linear-style rows.
//! Click a row to open it in the inspector (right sidebar).
//!
//! Done tasks are archived out of the list by default, behind a footer toggle
//! that always states its count. The archive is a *client-side view filter*
//! over the rows `GET /tasks` already returned — not a query parameter. The
//! endpoint is shared with the orchestrator, the briefing generator and
//! `tasks status`, and a view preference does not belong in it; the rows are a
//! few hundred at most, so filtering locally is cheaper than a refetch per
//! toggle, and the server stays the single authority on which tasks exist and
//! in what order.

use gpui::prelude::*;
use gpui::{div, px, Context, SharedString};
use gpuikit::elements::context_menu::context_menu;
use gpuikit::elements::tooltip::tooltip;
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{Task, TaskState};

use crate::components::{status_badge, task_state_color, title_case};
use crate::projects::{self, ProjectFilter};
use crate::time;
use crate::workspace::Workspace;

/// Split the server's task list into what the Tasks section shows and how many
/// done tasks exist.
///
/// It **drops rows, never sorts them** — the server orders the list and the
/// client only filters, which is what keeps this compatible with any ordering
/// the server grows later. The count is of tasks that are *done*, not of tasks
/// currently hidden, so the footer's number does not move when the toggle
/// does.
///
/// The repo filter is applied **first**, so the footer's done count is the
/// count for the repo on screen: "3 done · Show" about work in a repository
/// this window is not showing would be a number with no referent.
///
/// `Rejected` is deliberately not archived: the ask was to archive done, and
/// rejected work is one predicate away — changing it should be a decision,
/// not a refactor.
fn archive<'a>(
    tasks: &'a [Task],
    filter: &ProjectFilter,
    show_done: bool,
) -> (Vec<&'a Task>, usize) {
    let in_repo = filter.apply(tasks);
    let done = in_repo
        .iter()
        .filter(|task| task.state == TaskState::Done)
        .count();
    let visible = in_repo
        .into_iter()
        .filter(|task| show_done || task.state != TaskState::Done)
        .collect();
    (visible, done)
}

impl Workspace {
    pub(crate) fn render_tasks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app_state.read(cx);
        let selected = self.selected_task.clone();

        let (visible, done_count) = archive(&state.tasks, &self.project_filter, self.show_done);
        // Rows name their repo only when the rows *on screen* disagree about
        // it — which keeps a single-repo window, and a window filtered to one
        // repo, pixel-identical to the one before multi-repo.
        let ambiguous = projects::rows_are_ambiguous(&visible);
        let rows: Vec<_> = visible
            .into_iter()
            .map(|task| {
                (
                    task.id.clone(),
                    task.gh_issue_number,
                    task.title.clone(),
                    task.state,
                    task.updated_at,
                    ambiguous
                        .then(|| projects::row_repo_name(&state.projects, task))
                        .flatten(),
                )
            })
            .collect();
        let empty = rows.is_empty() && state.loaded;
        let show_done = self.show_done;

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .id("tasks-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .py(px(4.))
                    .when(empty, |el| {
                        el.child(
                            div()
                                .p(px(16.))
                                .text_sm()
                                .text_color(theme.fg_muted())
                                // Everything there is, is archived — say so,
                                // rather than claiming there is nothing.
                                .child(if done_count > 0 {
                                    "Nothing open — every task here is done."
                                } else {
                                    "No tasks yet — the poller fills this from open GitHub issues."
                                }),
                        )
                    })
                    .children(rows.into_iter().map(
                        |(id, number, title, task_state, updated, repo)| {
                            let is_selected = selected.as_ref() == Some(&id);
                            let color = task_state_color(task_state);
                            let row = div()
                                // Keyed by task, not by index: with rows
                                // appearing and disappearing behind the
                                // toggle, index N is a different task before
                                // and after, and gpui treats a repeated id
                                // across frames as the same node.
                                .id(SharedString::from(format!("task-{id}")))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .mx(px(6.))
                                .px(px(10.))
                                .py(px(4.))
                                .rounded(px(5.))
                                .cursor_pointer()
                                .when(!is_selected, |el| {
                                    let hover_bg = theme.surface_secondary();
                                    el.hover(move |el| el.bg(hover_bg))
                                })
                                .when(is_selected, |el| el.bg(theme.surface_tertiary()))
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _event, _window, cx| {
                                        this.select_task(id.clone(), cx);
                                    }
                                }))
                                .child(
                                    div()
                                        .w(px(52.))
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.fg_muted())
                                        .child(format!("#{number}")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .overflow_hidden()
                                        .text_sm()
                                        .text_color(theme.fg())
                                        .truncate()
                                        .child(title),
                                )
                                // The repository, not the owner: rows are
                                // narrow and the owner is the half that
                                // repeats.
                                .when_some(repo, |el, repo| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(theme.fg_muted())
                                            .child(repo),
                                    )
                                })
                                .when(task_state != TaskState::Backlog, |el| {
                                    el.child(status_badge(title_case(task_state.as_str()), color))
                                })
                                .child(
                                    div()
                                        .w(px(36.))
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.fg_muted())
                                        .child(time::relative(updated)),
                                );
                            // Right-click offers everything the inspector
                            // does and more, greyed to this row's state.
                            // Keyed by task for the same reason the row
                            // is: index N is a different task once the
                            // archive toggle moves.
                            context_menu(SharedString::from(format!("row-menu-{id}")), row)
                                .menu(Workspace::row_menu(id, cx))
                        },
                    )),
            )
            // The archive's receipt. Present whenever any task is done —
            // including while they are shown, so the way back is the same
            // control as the way in. Hiding work is only a problem when it is
            // silent, and this is the sentence that keeps it from being.
            .when(done_count > 0, |el| {
                el.child(self.render_archive_footer(done_count, show_done, cx))
            })
    }

    /// The Tasks list's footer: "3 done · Show" / "3 done · Hide".
    fn render_archive_footer(
        &self,
        done_count: usize,
        show_done: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let verb = if show_done { "Hide" } else { "Show" };
        let hover_bg = theme.surface_secondary();

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .px(px(12.))
            .py(px(6.))
            .border_t_1()
            .border_color(theme.border_subtle())
            .child(
                div()
                    .id("toggle-show-done")
                    // A node reaches the a11y tree only with both an id and a
                    // role; the label is what names it once the row is read
                    // as one control rather than two words.
                    .role(gpui::Role::Button)
                    .aria_label(format!("{verb} done tasks"))
                    .aria_keyshortcuts("⇧⌘D")
                    .tooltip(tooltip(format!("{verb} done tasks (⇧⌘D)")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(move |el| el.bg(hover_bg))
                    .text_xs()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.toggle_show_done(cx);
                    }))
                    .child(
                        div()
                            .text_color(theme.fg_muted())
                            .child(format!("{done_count} done")),
                    )
                    .child(div().text_color(theme.fg_muted()).child("·"))
                    .child(div().text_color(theme.fg()).child(verb)),
            )
            .child(div().flex_1())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{GhState, ProjectId, TaskId};

    use super::*;

    fn task(number: u64, state: TaskState) -> Task {
        task_in("proj-1", number, state)
    }

    fn task_in(project: &str, number: u64, state: TaskState) -> Task {
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
        }
    }

    fn numbers(tasks: &[&Task]) -> Vec<u64> {
        tasks.iter().map(|task| task.gh_issue_number).collect()
    }

    #[test]
    fn done_tasks_are_archived_by_default() {
        let tasks = [
            task(1, TaskState::Queued),
            task(2, TaskState::Done),
            task(3, TaskState::Building),
        ];
        let (visible, done) = archive(&tasks, &ProjectFilter::All, false);
        assert_eq!(numbers(&visible), [1, 3]);
        assert_eq!(done, 1);
    }

    /// The toggle restores them where the server put them — this filters, it
    /// does not sort, so whatever ordering the server ships survives it.
    #[test]
    fn showing_done_restores_them_in_the_servers_order() {
        let tasks = [
            task(1, TaskState::Done),
            task(2, TaskState::Queued),
            task(3, TaskState::Done),
        ];
        let (visible, _) = archive(&tasks, &ProjectFilter::All, true);
        assert_eq!(numbers(&visible), [1, 2, 3]);
    }

    /// The footer reports what is *done*, not what is currently hidden, so
    /// the number does not move when the toggle does.
    #[test]
    fn the_count_is_stable_across_the_toggle() {
        let tasks = [
            task(1, TaskState::Done),
            task(2, TaskState::Done),
            task(3, TaskState::InReview),
        ];
        assert_eq!(archive(&tasks, &ProjectFilter::All, false).1, 2);
        assert_eq!(archive(&tasks, &ProjectFilter::All, true).1, 2);
    }

    /// The repo filter is applied before the count, so the footer's number is
    /// the count for the repo on screen rather than for the whole server.
    #[test]
    fn the_done_count_is_the_count_for_the_repo_on_screen() {
        let tasks = [
            task_in("proj-1", 1, TaskState::Done),
            task_in("proj-2", 2, TaskState::Done),
            task_in("proj-1", 3, TaskState::Queued),
        ];
        let filter = ProjectFilter::One(ProjectId::from_raw("proj-1"));
        let (visible, done) = archive(&tasks, &filter, false);
        assert_eq!(numbers(&visible), [3]);
        assert_eq!(done, 1, "the other repo's done task is not this count");
        assert_eq!(archive(&tasks, &ProjectFilter::All, false).1, 2);
    }

    /// Rejected is not done. It stays in the list, and it is not in the count
    /// the footer offers to reveal.
    #[test]
    fn rejected_is_not_archived() {
        let tasks = [task(1, TaskState::Rejected), task(2, TaskState::Done)];
        let (visible, done) = archive(&tasks, &ProjectFilter::All, false);
        assert_eq!(numbers(&visible), [1]);
        assert_eq!(done, 1);
    }
}
