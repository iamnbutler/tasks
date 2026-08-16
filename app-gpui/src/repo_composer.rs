//! The "Add Repo" window.
//!
//! A small separate OS window, modelled on [`crate::issue_composer`]: one
//! field, no chrome of its own beyond the system title bar, cmd-enter sends and
//! closes. The draft `InputState` is owned by the workspace, so escape or
//! closing the window keeps the text.
//!
//! It deliberately does **not** parse the slug. The server normalizes
//! `owner/repo` (and refuses a duplicate case-insensitively), and a client-side
//! parser would be a second one to keep in step with it. The one split here is
//! the `/` the API's two fields require, and anything that does not have
//! exactly one comes back as the server's own refusal.

use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, Context, Entity, Focusable, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowOptions,
};
use gpuikit::elements::input::text_area;
use gpuikit::input::{InputState, InputStateEvent};
use gpuikit::theme::{ActiveTheme, Themeable};

use crate::state::AppState;
use crate::workspace::Workspace;

const FONT: &str = "Menlo";

pub fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Add Repo".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(460.), px(180.)),
            cx,
        ))),
        ..Default::default()
    }
}

/// `owner/repo` split into the two halves `POST /projects` takes.
///
/// Trimming only — everything else the server decides, so there is one
/// normalizer rather than two that can drift. `None` when there is no single
/// `/` to split on, which is the only thing worth refusing before a round trip:
/// the endpoint takes two fields and this window offers one box.
pub fn split_slug(draft: &str) -> Option<(String, String)> {
    let draft = draft.trim().trim_end_matches('/');
    let (owner, name) = draft.split_once('/')?;
    let (owner, name) = (owner.trim(), name.trim());
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

pub struct RepoComposer {
    /// Fallback send path if the main workspace is somehow gone.
    app_state: Entity<AppState>,
    input: Entity<InputState>,
    workspace: WeakEntity<Workspace>,
}

impl RepoComposer {
    pub fn new(
        app_state: Entity<AppState>,
        input: Entity<InputState>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputStateEvent, window, cx| match event {
                InputStateEvent::Submit => this.submit(window, cx),
                // The input's escape binding blurs it, and in this window
                // nothing else takes focus — a blur is escape. The draft stays
                // in the workspace-owned input.
                InputStateEvent::Blur => window.remove_window(),
                _ => {}
            },
        )
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        Self {
            app_state,
            input,
            workspace,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).content().trim().to_string();
        if draft.is_empty() {
            return;
        }
        let Some((owner, name)) = split_slug(&draft) else {
            self.app_state.update(cx, |state, cx| {
                state.report(format!("expected owner/repo, got {draft}"), cx)
            });
            return;
        };
        self.input.update(cx, |input, cx| input.set_content("", cx));
        // Through the workspace, so it can select the new repo once a snapshot
        // contains it: the client applies snapshots rather than responses, so
        // at the moment this window closes there is no id yet — only the slug.
        if self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.add_project(owner.clone(), name.clone(), cx)
            })
            .is_err()
        {
            self.app_state
                .update(cx, |state, cx| state.add_project(owner, name, cx));
        }
        window.remove_window();
    }
}

impl Render for RepoComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let has_slug = split_slug(self.input.read(cx).content()).is_some();

        div()
            .key_context("RepoComposer")
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT)
            .bg(theme.bg())
            .text_color(theme.fg())
            .text_sm()
            .child(
                div()
                    .flex_none()
                    .px(px(12.))
                    .pt(px(10.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child("Repository to track, as owner/repo"),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .p(px(8.))
                    .child(text_area(&self.input, cx).size_full()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(theme.border_subtle())
                    .text_xs()
                    .text_color(theme.fg_muted())
                    // The sentence that keeps "add a repo with 11,000 issues"
                    // from reading as a threat: intake is not dispatch.
                    .child("Its open issues land in the backlog — nothing is dispatched")
                    .child(
                        div()
                            .id("add-repo")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .map(|el| {
                                if has_slug {
                                    el.cursor_pointer()
                                        .text_color(theme.fg())
                                        .hover({
                                            let hover_bg = theme.surface_secondary();
                                            move |el| el.bg(hover_bg)
                                        })
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.submit(window, cx);
                                        }))
                                } else {
                                    el.text_color(theme.fg_muted()).opacity(0.5)
                                }
                            })
                            .child("Add repo ⌘↩"),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_splits_into_the_two_fields_the_api_takes() {
        assert_eq!(
            split_slug("iamnbutler/tasks"),
            Some(("iamnbutler".into(), "tasks".into()))
        );
        assert_eq!(
            split_slug("  iamnbutler / tasks  "),
            Some(("iamnbutler".into(), "tasks".into()))
        );
        assert_eq!(
            split_slug("iamnbutler/tasks/"),
            Some(("iamnbutler".into(), "tasks".into()))
        );
    }

    /// Only what the two-field endpoint cannot be given at all. Case,
    /// `.git` suffixes and every other normalization are the server's, so
    /// there is one normalizer rather than two that drift.
    #[test]
    fn only_the_unsplittable_is_refused_here() {
        assert_eq!(split_slug("tasks"), None);
        assert_eq!(split_slug(""), None);
        assert_eq!(split_slug("/tasks"), None);
        assert_eq!(split_slug("iamnbutler/"), None);
        assert_eq!(split_slug("github.com/iamnbutler/tasks"), None);
        // Left for the server, deliberately: it strips the suffix, and a
        // client that also did would be the second implementation.
        assert_eq!(
            split_slug("iamnbutler/tasks.git"),
            Some(("iamnbutler".into(), "tasks.git".into()))
        );
    }
}
