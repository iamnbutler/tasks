//! The Changes tab: what a build did to the repository, from build
//! artifacts the server owns — never from GitHub (house rule: GitHub-owned
//! facts are queried at decision time by the *server*; this app renders
//! Tasks-owned state).
//!
//! What can honestly be attributed today: the **running** build while this
//! task is `Building` (the build lane is serial, so there is exactly one),
//! and a preserved bundle (whose `task_ids` the server joins). Historical
//! builds carry no task linkage on the wire — a build is a batch of specs —
//! so they cannot be listed here without inventing a mapping; a `task_ids`
//! field on `Build`, computed server-side like the bundle's, is the
//! follow-up that unlocks the history view (docs/plans/2026-08-17-v3-ui.md
//! §8).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{Build, BuildStatus, TaskState};

use crate::components::{status_badge, task_state_color, title_case};
use crate::time;
use crate::workspace::Workspace;

/// The Builder's own claim about its work, stated as a trailer in
/// `SUMMARY.md`: `Verification: PASSED|FAILED|NOT RUN`. A **claim**, not a
/// check — this repo has no CI, so the build's own test run is the only
/// evidence a change works, and the tab attributes it as the builder's
/// statement. Parsed off the summary because that is where the pipeline
/// puts it (one sentence serving the PR body, the brief, and this tab, no
/// migration in between).
pub(crate) fn verification(summary: &str) -> Option<&str> {
    summary.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix("Verification:")
            .map(|rest| rest.trim())
            .filter(|rest| !rest.is_empty())
    })
}

impl Workspace {
    /// The Changes tab for the selected task.
    pub(crate) fn render_changes(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (build, state_name, bundle) = {
            let state = self.app_state.read(cx);
            let Some(task) = self.selected_task.as_ref().and_then(|id| state.task(id)) else {
                return div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(16.))
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child("That task is no longer in the working set.")
                    .into_any_element();
            };
            let build = (task.state == TaskState::Building)
                .then(|| {
                    state
                        .builds
                        .iter()
                        .find(|build| build.status == BuildStatus::Running)
                        .cloned()
                })
                .flatten();
            (build, task.state, state.bundle_for_task(&task.id).cloned())
        };

        let mut cards: Vec<AnyElement> = Vec::new();
        if let Some(build) = build {
            cards.push(self.render_build_card(build, cx));
        }
        if let Some(bundle) = bundle {
            cards.push(self.render_bundle(bundle, cx));
        }

        // Nothing attributable: say what would appear, and why history
        // doesn't yet.
        if cards.is_empty() {
            let sentence = match state_name {
                TaskState::AwaitingMerge => {
                    "This task's build has opened a pull request — the server is \
                     watching for the merge. Linking the build's summary here needs \
                     task linkage on builds (a noted follow-up)."
                }
                _ => {
                    "A build appears here while the pipeline is building this task, \
                     with its branch, summary and the builder's verification claim. \
                     Build history needs task linkage on builds (a noted follow-up)."
                }
            };
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .p(px(16.))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.fg())
                        .child("No changes to show"),
                )
                .child(
                    div()
                        .max_w(px(460.))
                        .text_center()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(sentence),
                )
                .into_any_element();
        }

        Self::tab_pane("task-changes-scroll")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .max_w(px(760.))
                    .children(cards),
            )
            .into_any_element()
    }

    /// One build's card: identity, status, files, the summary, and the
    /// verification trailer surfaced as the claim it is.
    fn render_build_card(&self, build: Build, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let pr_url = {
            let state = self.app_state.read(cx);
            build.pr_number.and_then(|number| {
                state
                    .projects
                    .iter()
                    .find(|project| project.id == build.project_id)
                    .map(|project| {
                        format!(
                            "https://github.com/{}/{}/pull/{number}",
                            project.repo_owner, project.repo_name
                        )
                    })
            })
        };
        let elapsed = build
            .started_at
            .map(|started| format!("running · {}", time::elapsed(started)));
        let claim = build.summary.as_deref().and_then(verification);

        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border_subtle())
            .bg(theme.surface_secondary())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(status_badge(
                        title_case(build.status.as_str()),
                        task_state_color(TaskState::Building),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .truncate()
                            .text_sm()
                            .text_color(theme.fg())
                            .child(build.branch.clone()),
                    )
                    .children(elapsed.map(|elapsed| {
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.accent())
                            .child(elapsed)
                    })),
            );

        if let Some(url) = pr_url {
            card = card.child(
                div()
                    .id("build-pr-link")
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .cursor_pointer()
                    .hover({
                        let fg = theme.fg();
                        move |el| el.text_color(fg)
                    })
                    .on_click({
                        let url = url.clone();
                        move |_event, _window, cx| cx.open_url(&url)
                    })
                    .child(url),
            );
        }

        // The builder's own claim, attributed as one. `Unreported` (no
        // trailer) reads as "no run on record" — the direction a mistake
        // here has to fall.
        card = card.child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.))
                .text_xs()
                .child(div().text_color(theme.fg_muted()).child("Verification:"))
                .child(match claim {
                    Some(claim) => div()
                        .text_color(match claim {
                            "PASSED" => gpui::hsla(135. / 360., 0.55, 0.52, 1.),
                            "FAILED" => gpui::hsla(0., 0.75, 0.55, 1.),
                            _ => theme.fg(),
                        })
                        .child(format!("{claim} (builder's claim)")),
                    None => div()
                        .text_color(theme.fg_muted())
                        .child("not reported — no run on record"),
                }),
        );

        if !build.files_touched.is_empty() {
            card =
                card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .pt(px(4.))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(format!("FILES · {}", build.files_touched.len())),
                        )
                        .children(build.files_touched.iter().map(|file| {
                            div().text_xs().text_color(theme.fg()).child(file.clone())
                        })),
                );
        }

        if let Some(summary) = build
            .summary
            .as_ref()
            .filter(|summary| !summary.trim().is_empty())
        {
            let entity =
                self.markdown_cache()
                    .entity(format!("build-summary:{}", build.id), summary, cx);
            card = card.child(
                div()
                    .pt(px(4.))
                    .text_sm()
                    .text_color(theme.fg())
                    .child(crate::components::markdown_block(&entity, cx)),
            );
        }

        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trailer_parses_wherever_it_sits() {
        assert_eq!(
            verification("did things\n\nVerification: PASSED\n"),
            Some("PASSED")
        );
        assert_eq!(verification("Verification: NOT RUN"), Some("NOT RUN"));
        assert_eq!(verification("  Verification:   FAILED  "), Some("FAILED"));
    }

    /// The last trailer wins — a summary quoting an earlier build's line
    /// must not outrank the build's own statement at the end.
    #[test]
    fn the_last_trailer_wins() {
        assert_eq!(
            verification("Verification: FAILED\nfixed it\nVerification: PASSED"),
            Some("PASSED")
        );
    }

    /// No trailer parses as no claim — never as a default verdict.
    #[test]
    fn absence_is_unreported_not_a_verdict() {
        assert_eq!(verification("shipped some code"), None);
        assert_eq!(verification("Verification:"), None);
        assert_eq!(verification("Verification:   "), None);
    }
}
