//! The Home section: the three LLM briefing slots plus what needs a human,
//! reading-width capped. Plain text for now — markdown rendering is its own
//! later slice.

use gpui::prelude::*;
use gpui::{div, px, Context};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::TaskState;

use crate::time;
use crate::workspace::Workspace;

impl Workspace {
    pub(crate) fn render_home(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app_state.read(cx);

        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .w_full()
            .max_w(px(760.))
            .mx_auto()
            .p(px(16.));

        // Needs you: specs awaiting a verdict.
        let waiting: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| task.state == TaskState::InReview)
            .map(|task| (task.title.clone(), task.updated_at))
            .collect();
        if !waiting.is_empty() {
            let mut section = div().flex().flex_col().gap(px(4.)).child(
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child("NEEDS YOU"),
            );
            for (title, updated) in waiting {
                section = section.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(theme.surface())
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.fg())
                                        .truncate()
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.fg_muted())
                                        .child("Spec awaiting your verdict"),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(format!("waiting {}", time::relative(updated))),
                        ),
                );
            }
            column = column.child(section);
        }

        // Briefing slots, in server display order.
        for briefing in &state.briefings {
            let label = crate::components::title_case(briefing.section.as_str()).to_uppercase();
            let provenance = match (&briefing.generated_at, briefing.regenerating) {
                (Some(at), true) => format!("as of {} ago · refreshing…", time::relative(*at)),
                (Some(at), false) => format!("as of {} ago", time::relative(*at)),
                (None, true) => "Writing the first briefing…".to_string(),
                (None, false) => "No briefing yet".to_string(),
            };
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().text_xs().text_color(theme.fg_muted()).child(label))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.fg_muted())
                                    .child(provenance),
                            ),
                    )
                    .when_some(briefing.content.clone(), |el, content| {
                        el.child(div().text_sm().text_color(theme.fg()).child(content))
                    }),
            );
        }

        if state.briefings.is_empty() && state.loaded {
            column = column.child(
                div()
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child("No briefings yet."),
            );
        }

        div()
            .id("home-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}
