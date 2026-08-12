//! The Tasks section: every task in the working set, Linear-style rows.
//! Click a row to open it in the inspector (right sidebar).

use gpui::prelude::*;
use gpui::{div, px, Context};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::TaskState;

use crate::components::{status_badge, task_state_color, title_case};
use crate::time;
use crate::workspace::Workspace;

impl Workspace {
    pub(crate) fn render_tasks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app_state.read(cx);
        let selected = self.selected_task.clone();

        let rows: Vec<_> = state
            .tasks
            .iter()
            .map(|task| {
                (
                    task.id.clone(),
                    task.gh_issue_number,
                    task.title.clone(),
                    task.state,
                    task.updated_at,
                )
            })
            .collect();
        let empty = rows.is_empty() && state.loaded;

        div()
            .id("tasks-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .py(px(4.))
            .when(empty, |el| {
                el.child(
                    div()
                        .p(px(16.))
                        .text_sm()
                        .text_color(theme.fg_muted())
                        .child("No tasks yet — the poller fills this from open GitHub issues."),
                )
            })
            .children(rows.into_iter().enumerate().map(
                |(ix, (id, number, title, task_state, updated))| {
                    let is_selected = selected.as_ref() == Some(&id);
                    let color = task_state_color(task_state);
                    div()
                        .id(ix)
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
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.select_task(id.clone(), cx);
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
                        )
                },
            ))
    }
}
