//! The Queue section: picked-up work grouped in attention order, mirroring
//! the Swift app — Needs you / Running / Building / Up next / Ready to build.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{BuildStatus, TaskId, TaskState};

use crate::time;
use crate::workspace::Workspace;

struct QueueRow {
    task_id: TaskId,
    number: u64,
    title: String,
    /// Trailing accessory: complexity word, or a live elapsed clock.
    trailing: Option<String>,
    /// Trailing is a live clock (styled as active work).
    live: bool,
}

impl Workspace {
    pub(crate) fn render_queue(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app_state.read(cx);
        let selected = self.selected_task.clone();

        let row = |task: &tasks_client::api::models::Task| QueueRow {
            task_id: task.id.clone(),
            number: task.gh_issue_number,
            title: task.title.clone(),
            trailing: None,
            live: false,
        };

        let mut groups: Vec<(&'static str, Vec<QueueRow>)> = Vec::new();

        let needs_you: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::InReview)
            .map(|task| {
                let mut item = row(task);
                item.trailing = state
                    .latest_spec(&task.id)
                    .map(|spec| spec.complexity.as_str().to_string());
                item
            })
            .collect();
        groups.push(("Needs you", needs_you));

        let running: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::Scouting)
            .map(|task| {
                let mut item = row(task);
                if let Some(session) = state.running_session(&task.id) {
                    item.trailing = Some(time::elapsed(session.started_at));
                    item.live = true;
                }
                item
            })
            .collect();
        groups.push(("Running", running));

        let building: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::Building)
            .map(|task| {
                let mut item = row(task);
                if let Some(build) = state
                    .builds
                    .iter()
                    .find(|build| build.status == BuildStatus::Running)
                {
                    if let Some(started) = build.started_at {
                        item.trailing = Some(time::elapsed(started));
                        item.live = true;
                    }
                }
                item
            })
            .collect();
        groups.push(("Building", building));

        let up_next: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::Queued)
            .map(row)
            .collect();
        groups.push(("Up next", up_next));

        let ready: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::ReadyToBuild)
            .map(|task| {
                let mut item = row(task);
                item.trailing = state
                    .latest_spec(&task.id)
                    .map(|spec| spec.complexity.as_str().to_string());
                item
            })
            .collect();
        groups.push(("Ready to build", ready));

        let all_empty = groups.iter().all(|(_, rows)| rows.is_empty()) && state.loaded;

        let mut list = div()
            .id("queue-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .py(px(4.));

        if all_empty {
            list = list.child(
                div()
                    .p(px(16.))
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child("Nothing queued — pick tasks up from the Tasks list."),
            );
        }

        for (label, rows) in groups {
            if rows.is_empty() {
                continue;
            }
            list = list.child(
                div()
                    .px(px(16.))
                    .pt(px(10.))
                    .pb(px(4.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(label.to_uppercase()),
            );
            for (ix, item) in rows.into_iter().enumerate() {
                list = list.child(self.queue_row(label, ix, item, selected.as_ref(), cx));
            }
        }

        list
    }

    fn queue_row(
        &self,
        group: &'static str,
        ix: usize,
        item: QueueRow,
        selected: Option<&TaskId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let is_selected = selected == Some(&item.task_id);
        let task_id = item.task_id.clone();

        div()
            .id((group, ix))
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
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_task(task_id.clone(), cx);
            }))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.fg())
                            .truncate()
                            .child(item.title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child(format!("#{}", item.number)),
                    ),
            )
            .when_some(item.trailing, |el, trailing| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(if item.live {
                            theme.accent()
                        } else {
                            theme.fg_muted()
                        })
                        .child(trailing),
                )
            })
            .into_any_element()
    }
}
