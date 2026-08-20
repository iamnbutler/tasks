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
use tasks_client::api::models::{Build, BuildStatus, TaskState, Verification, VerificationStatus};

use crate::components::{status_badge, task_state_color, title_case};
use crate::time;
use crate::workspace::Workspace;

/// How the Changes tab renders a build's verification.
///
/// It reads the **structured field** the Builder supervisor stamps on the
/// build, not a trailer parsed out of `SUMMARY.md`. That parser was a second
/// one, beside the server's, over prose the graded agent wrote; both are gone.
/// What is left is a check: the supervisor ran the project's own suite inside
/// the VM, and a failing suite never opened a pull request at all.
pub(crate) fn verification_label(
    verification: Option<&Verification>,
) -> (String, VerificationTone) {
    let Some(v) = verification else {
        return (
            "not reported — no run on record".into(),
            VerificationTone::Absent,
        );
    };
    match v.status {
        VerificationStatus::Passed => (
            "PASSED — the supervisor ran this project's suite".into(),
            VerificationTone::Green,
        ),
        VerificationStatus::Undeclared => (
            "none — this project declares no `.tasks/verify`".into(),
            VerificationTone::Absent,
        ),
        VerificationStatus::Unavailable => (
            "unavailable — the suite could not be run".into(),
            VerificationTone::Absent,
        ),
        VerificationStatus::TimedOut => (
            "timed out — the suite was killed by its budget".into(),
            VerificationTone::Absent,
        ),
    }
}

/// How a verification reads at a glance.
///
/// **There is no "bad" tone**, for the same structural reason
/// [`VerificationStatus`] has no `Failed` variant: a red suite fails the build
/// inside the VM, so this tab can never be asked to render one. Everything that
/// is not a pass is an absence of evidence, and they all read the same way —
/// which is the right way, because they all mean the same thing to whoever is
/// deciding whether to merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationTone {
    Green,
    Absent,
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
                     with its branch, summary and the supervisor's verification. \
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
    /// verification surfaced as the check it is.
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
        let claim = verification_label(build.verification.as_ref());

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

        // A check rather than a claim, and `None` reads as "no run on
        // record" — the direction a mistake here has to fall.
        let (label, tone) = claim;
        card = card.child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.))
                .text_xs()
                .child(div().text_color(theme.fg_muted()).child("Verification:"))
                .child(
                    div()
                        .text_color(match tone {
                            VerificationTone::Green => gpui::hsla(135. / 360., 0.55, 0.52, 1.),
                            VerificationTone::Absent => theme.fg_muted(),
                        })
                        .child(label),
                ),
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

    fn v(status: VerificationStatus) -> Verification {
        Verification {
            status,
            detail: "make test-ci (gate abc1234)".into(),
        }
    }

    /// Exactly one state renders green, and it is the one the supervisor's own
    /// run produced.
    #[test]
    fn only_a_passing_run_reads_green() {
        let (label, tone) = verification_label(Some(&v(VerificationStatus::Passed)));
        assert_eq!(tone, VerificationTone::Green);
        assert!(label.contains("PASSED"), "{label}");
        // And it says whose run it was — the whole point of replacing a trailer
        // the agent wrote with a check the supervisor ran.
        assert!(label.contains("supervisor"), "{label}");

        for status in [
            VerificationStatus::Undeclared,
            VerificationStatus::Unavailable,
            VerificationStatus::TimedOut,
        ] {
            let (label, tone) = verification_label(Some(&v(status)));
            assert_eq!(tone, VerificationTone::Absent, "{status}: {label}");
        }
    }

    /// No field parses as no run — never as a default verdict, and never as a
    /// failure the tab has no way to be handed.
    #[test]
    fn absence_is_no_run_on_record_not_a_verdict() {
        let (label, tone) = verification_label(None);
        assert_eq!(tone, VerificationTone::Absent);
        assert!(label.contains("no run on record"), "{label}");
    }

    /// Each not-green state says which one it is: "declares no suite" and "the
    /// suite was killed" send a reader to different fixes.
    #[test]
    fn the_not_green_states_are_distinguishable() {
        let labels: Vec<String> = [
            VerificationStatus::Undeclared,
            VerificationStatus::Unavailable,
            VerificationStatus::TimedOut,
        ]
        .into_iter()
        .map(|s| verification_label(Some(&v(s))).0)
        .collect();
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }
}
