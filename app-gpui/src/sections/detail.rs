//! The inspector: detail + actions for the selected task, rendered in the
//! right sidebar. The server is the authority on which transitions are
//! legal — buttons are offered by state, and a rejected action surfaces the
//! server's own error message in the banner.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, ClickEvent, Context, Hsla};
use gpuikit::elements::input::text_area;
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{Complexity, GhState, SpecId, SpecQueueStatus, TaskId, TaskState};

use crate::components::{status_badge, task_state_color, title_case};
use crate::time;
use crate::workspace::Workspace;

/// Owned projection of the selected task — extracted up front so no borrow
/// of the app state entity is held while listeners are created.
struct TaskView {
    id: TaskId,
    title: String,
    number: u64,
    state: TaskState,
    gh_state: GhState,
    labels: Vec<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
    body: String,
    github_url: Option<String>,
    /// `(spec id, complexity, content)` when the task sits in review with a
    /// pending verdict.
    pending_spec: Option<(SpecId, Complexity, String)>,
}

impl Workspace {
    /// Content for the right sidebar. Placeholder when nothing is selected.
    pub(crate) fn render_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let view: Option<TaskView> = {
            let state = self.app_state.read(cx);
            self.selected_task
                .as_ref()
                .and_then(|id| state.task(id))
                .map(|task| TaskView {
                    id: task.id.clone(),
                    title: task.title.clone(),
                    number: task.gh_issue_number,
                    state: task.state,
                    gh_state: task.gh_state,
                    labels: task.labels.clone(),
                    updated_at: task.updated_at,
                    body: task.body.clone(),
                    github_url: state.project(task).map(|project| {
                        format!(
                            "https://github.com/{}/{}/issues/{}",
                            project.repo_owner, project.repo_name, task.gh_issue_number
                        )
                    }),
                    pending_spec: (task.state == TaskState::InReview)
                        .then(|| state.latest_spec(&task.id))
                        .flatten()
                        .filter(|spec| {
                            state.spec_queue.iter().any(|item| {
                                item.entry.spec_id == spec.id
                                    && item.entry.status == SpecQueueStatus::PendingReview
                            })
                        })
                        .map(|spec| (spec.id.clone(), spec.complexity, spec.content.clone())),
                })
        };

        let theme = cx.theme().clone();
        let Some(task) = view else {
            return div()
                .p(px(12.))
                .text_sm()
                .text_color(theme.fg_muted())
                .child("Select a task to inspect it.")
                .into_any_element();
        };

        let mut pane = div()
            .id("inspector-scroll")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .p(px(12.))
            .gap(px(10.));

        pane = pane.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(theme.fg())
                        .child(task.title.clone()),
                )
                .child(
                    div()
                        .id("close-inspector")
                        .flex_none()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .hover(|el| el.opacity(0.7))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.clear_selection(cx);
                        }))
                        .child("✕"),
                ),
        );

        pane = pane.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(status_badge(
                    title_case(task.state.as_str()),
                    task_state_color(task.state),
                ))
                .child(status_badge(
                    task.gh_state.as_str().to_string(),
                    match task.gh_state {
                        GhState::Open => gpui::hsla(135. / 360., 0.55, 0.52, 1.),
                        GhState::Closed => gpui::hsla(280. / 360., 0.70, 0.68, 1.),
                    },
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(format!("#{}", task.number)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(format!("updated {}", time::relative(task.updated_at))),
                ),
        );

        if !task.labels.is_empty() {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(task.labels.join(", ")),
            );
        }

        // Actions by state; the server enforces legality.
        let mut actions = div().flex().flex_row().flex_wrap().gap(px(6.));
        let mut any_action = false;
        match task.state {
            TaskState::Backlog => {
                any_action = true;
                actions = actions
                    .child(self.action_button(
                        "queue-task",
                        "Add to Queue",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.queue_task(id, cx));
                            }
                        }),
                        cx,
                    ))
                    .child(self.action_button(
                        "scout-task",
                        "Scout Now",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.scout_task_now(id, cx));
                            }
                        }),
                        cx,
                    ));
            }
            TaskState::Queued => {
                any_action = true;
                actions = actions
                    .child(self.action_button(
                        "scout-task",
                        "Scout Now",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.scout_task_now(id, cx));
                            }
                        }),
                        cx,
                    ))
                    .child(self.action_button(
                        "dequeue-task",
                        "Remove from Queue",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.dequeue_task(id, cx));
                            }
                        }),
                        cx,
                    ));
            }
            _ => {}
        }
        if let Some((spec_id, _, _)) = &task.pending_spec {
            any_action = true;
            actions = actions.child(self.action_button(
                "approve-spec",
                "Approve",
                Some(gpui::hsla(135. / 360., 0.55, 0.45, 1.)),
                cx.listener({
                    let id = spec_id.clone();
                    move |this, _: &ClickEvent, _window, cx| {
                        let id = id.clone();
                        this.app_state.update(cx, |state, cx| {
                            state.review_spec(id, SpecQueueStatus::Approved, None, cx)
                        });
                    }
                }),
                cx,
            ));
        }
        if let Some(url) = task.github_url.clone() {
            any_action = true;
            actions = actions.child(self.action_button(
                "open-github",
                "Open on GitHub",
                None,
                move |_: &ClickEvent, _window, cx| cx.open_url(&url),
                cx,
            ));
        }
        if any_action {
            pane = pane.child(actions);
        }

        // Review form: one draft, three exits. "Request Changes" renders a
        // needs_revision verdict — the text travels with the spec to the
        // re-scout. "Ask" routes the text (plus task/spec context) into the
        // orchestrator conversation for anything that isn't a verdict yet:
        // "is this already done?", "should we close this?". Reject lives
        // here, quieter than Approve — in practice you ask before you reject.
        if let Some((spec_id, _, _)) = &task.pending_spec {
            let has_text = !self.review_input.read(cx).content().trim().is_empty();
            pane = pane.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .h(px(72.))
                            .p(px(4.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .bg(theme.bg())
                            .text_sm()
                            .child(text_area(&self.review_input, cx).size_full()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(self.form_button(
                                "request-changes",
                                "Request Changes",
                                gpui::hsla(35. / 360., 0.80, 0.55, 1.),
                                has_text,
                                cx.listener({
                                    let id = spec_id.clone();
                                    move |this, _: &ClickEvent, _window, cx| {
                                        let Some(text) = this.take_review_draft(cx) else {
                                            return;
                                        };
                                        let id = id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.review_spec(
                                                id,
                                                SpecQueueStatus::NeedsRevision,
                                                Some(text),
                                                cx,
                                            )
                                        });
                                    }
                                }),
                                cx,
                            ))
                            .child(self.form_button(
                                "ask-orchestrator",
                                "Ask Orchestrator",
                                theme.fg(),
                                has_text,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.ask_about_selected_spec(cx);
                                }),
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(self.form_button(
                                "reject-spec",
                                "Reject",
                                gpui::hsla(0., 0.75, 0.55, 1.),
                                true,
                                cx.listener({
                                    let id = spec_id.clone();
                                    move |this, _: &ClickEvent, _window, cx| {
                                        let feedback = this.take_review_draft(cx);
                                        let id = id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.review_spec(
                                                id,
                                                SpecQueueStatus::Rejected,
                                                feedback,
                                                cx,
                                            )
                                        });
                                    }
                                }),
                                cx,
                            )),
                    ),
            );
        }

        // Specs and issue bodies are markdown at the source (agent output,
        // GitHub issues) — render them as such, through the shared cache.
        if let Some((spec_id, complexity, content)) = task.pending_spec {
            let entity = self
                .markdown_cache()
                .entity(format!("spec:{spec_id}"), &content, cx);
            pane = pane
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(format!("SPEC · {}", complexity.as_str().to_uppercase())),
                )
                .child(
                    div()
                        .p(px(8.))
                        .rounded(px(6.))
                        .bg(theme.bg())
                        .text_sm()
                        .text_color(theme.fg())
                        .child(crate::components::markdown_block(&entity, cx)),
                );
        } else if !task.body.is_empty() {
            let entity = self
                .markdown_cache()
                .entity(format!("task:{}", task.id), &task.body, cx);
            pane = pane.child(
                div()
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child(crate::components::markdown_block(&entity, cx)),
            );
        }

        pane.into_any_element()
    }

    /// The trimmed review draft, clearing the composer — `None` if empty.
    pub(crate) fn take_review_draft(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let text = self.review_input.read(cx).content().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.review_input
            .update(cx, |input, cx| input.set_content("", cx));
        Some(text)
    }

    /// A submit button for the review form; renders inert and dimmed until
    /// the draft has text.
    fn form_button(
        &self,
        id: &'static str,
        label: &'static str,
        color: Hsla,
        enabled: bool,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let base = div()
            .id(id)
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .text_xs();
        if enabled {
            base.text_color(color)
                .cursor_pointer()
                .hover({
                    let hover_bg = theme.surface_secondary();
                    move |el| el.bg(hover_bg)
                })
                .on_click(on_click)
                .child(label)
                .into_any_element()
        } else {
            base.text_color(theme.fg_muted())
                .opacity(0.5)
                .child(label)
                .into_any_element()
        }
    }

    fn action_button(
        &self,
        id: &'static str,
        label: &'static str,
        color: Option<Hsla>,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let text = color.unwrap_or_else(|| theme.fg());
        div()
            .id(id)
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .cursor_pointer()
            .text_xs()
            .text_color(text)
            .hover({
                let hover_bg = theme.surface_secondary();
                move |el| el.bg(hover_bg)
            })
            .on_click(on_click)
            .child(label)
            .into_any_element()
    }
}
