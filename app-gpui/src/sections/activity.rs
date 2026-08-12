//! The Activity section: the newest slice of the event log, newest first.
//! Sentences are built from the typed `EventPayload` — exhaustive on
//! purpose, so a new event kind is a compile error here, not a mystery row.

use gpui::prelude::*;
use gpui::{div, px, Context};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::events::EventPayload;

use crate::components::title_case;
use crate::time;
use crate::workspace::Workspace;

impl Workspace {
    pub(crate) fn render_activity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app_state.read(cx);

        let title_for = |task_id: &tasks_client::api::models::TaskId| {
            state
                .task(task_id)
                .map(|task| task.title.clone())
                .unwrap_or_else(|| task_id.to_string())
        };

        let rows: Vec<(i64, String, String)> = state
            .activity
            .iter()
            .map(|event| {
                let sentence = match &event.payload {
                    EventPayload::ProjectAdded { .. } => "Project added".to_string(),
                    EventPayload::TaskIngested { task_id, .. } => {
                        format!("Ingested “{}”", title_for(task_id))
                    }
                    EventPayload::TaskStateChanged { task_id, from, to } => format!(
                        "“{}” moved {} → {}",
                        title_for(task_id),
                        title_case(from.as_str()),
                        title_case(to.as_str())
                    ),
                    EventPayload::TaskGhStateChanged { task_id, gh_state } => format!(
                        "GitHub issue for “{}” is now {}",
                        title_for(task_id),
                        gh_state.as_str()
                    ),
                    EventPayload::SessionStarted { task_id, .. } => {
                        format!("Scout started on “{}”", title_for(task_id))
                    }
                    EventPayload::SessionCompleted {
                        task_id, status, ..
                    } => format!(
                        "Scout {} on “{}”",
                        title_case(status.as_str()).to_lowercase(),
                        title_for(task_id)
                    ),
                    EventPayload::SpecCreated { task_id, .. } => {
                        format!("Spec landed for “{}”", title_for(task_id))
                    }
                    EventPayload::SpecQueueStatusChanged { to, .. } => {
                        format!("Spec review: {}", title_case(to.as_str()))
                    }
                    EventPayload::QueueReordered { task_ids } => {
                        format!("Queue reordered ({} tasks)", task_ids.len())
                    }
                    EventPayload::SpecQueueReordered { spec_ids } => {
                        format!("Spec queue reordered ({} specs)", spec_ids.len())
                    }
                    EventPayload::BuildRequested { spec_ids, .. } => {
                        format!("Build requested over {} spec(s)", spec_ids.len())
                    }
                    EventPayload::BuildStarted { .. } => "Build started".to_string(),
                    EventPayload::BuildCompleted { status, .. } => {
                        format!("Build {}", status.as_str())
                    }
                    EventPayload::PullRequestOpened { pr_number, .. } => {
                        format!("Opened PR #{pr_number}")
                    }
                    EventPayload::OrchestratorMessage { role, .. } => {
                        format!("Orchestrator: {} turn", role.as_str())
                    }
                    EventPayload::ModeChanged { from, to } => {
                        format!("Mode {} → {}", from.as_str(), to.as_str())
                    }
                    EventPayload::BriefingUpdated { section } => {
                        format!("Briefing updated: {}", title_case(section.as_str()))
                    }
                    EventPayload::Note { source, message } => format!("[{source}] {message}"),
                };
                (event.seq, sentence, time::relative(event.timestamp))
            })
            .collect();

        div()
            .id("activity-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .py(px(4.))
            .when(rows.is_empty() && state.loaded, |el| {
                el.child(
                    div()
                        .p(px(16.))
                        .text_sm()
                        .text_color(theme.fg_muted())
                        .child("Nothing has happened yet."),
                )
            })
            .children(rows.into_iter().map(|(seq, sentence, when)| {
                div()
                    .id(seq as usize)
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(8.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(4.))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(theme.fg())
                            .child(sentence),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child(when),
                    )
            }))
    }
}
