//! The "Add Repo" window.
//!
//! One `owner/repo` field, modelled on [`crate::issue_composer`]: a small
//! separate OS window, the draft `InputState` owned by the workspace so escape
//! keeps the text, cmd-enter submits.
//!
//! **It deliberately does not parse the slug.** The server normalizes
//! `owner/repo` (and refuses a duplicate case-insensitively), so a client-side
//! parser would be a second one to keep in step — the split here is the
//! minimum needed to fill two request fields, and anything it gets wrong the
//! server answers for in the banner.

use gpui::prelude::*;
use gpui::{
    div, px, size, App, Bounds, Context, Entity, Focusable, MouseButton, TitlebarOptions,
    WeakEntity, Window, WindowBounds, WindowOptions,
};
use gpuikit::input::{InputState, InputStateEvent};
use gpuikit::theme::{ActiveTheme, Themeable};

use crate::components::text_field;
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
            size(px(460.), px(160.)),
            cx,
        ))),
        ..Default::default()
    }
}

/// Split `owner/repo` into its two request fields.
///
/// The whole of the client-side parsing, on purpose: everything else — the
/// trimming, the duplicate check, the case-insensitive match — is the server's,
/// and stating it twice is how the two answers start to differ.
pub fn split_slug(draft: &str) -> Option<(String, String)> {
    let (owner, name) = draft.trim().trim_matches('/').split_once('/')?;
    let (owner, name) = (owner.trim(), name.trim().trim_end_matches('/'));
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| (owner.to_string(), name.to_string()))
}

pub struct RepoComposer {
    /// Fallback path if the main workspace is somehow gone.
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
                // As in the issue composer: nothing else in this window takes
                // focus, so a blur is escape. The draft stays put.
                InputStateEvent::Blur => window.remove_window(),
                _ => {}
            },
        )
        .detach();
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        window.focus(&input.focus_handle(cx), cx);
        Self {
            app_state,
            input,
            workspace,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).content().to_string();
        let Some((owner, name)) = split_slug(&draft) else {
            return;
        };
        self.input.update(cx, |input, cx| input.set_content("", cx));
        // Park the intent on the slug: the client applies snapshots rather than
        // responses, so at the moment this window closes there is no id yet.
        // The workspace picks it up out of the first snapshot that holds it.
        let slug = format!("{owner}/{name}");
        if self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.add_repo(owner.clone(), name.clone(), slug, cx);
            })
            .is_err()
        {
            self.app_state
                .update(cx, |state, cx| state.create_project(owner, name, cx));
        }
        window.remove_window();
    }
}

impl Render for RepoComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ready = split_slug(self.input.read(cx).content()).is_some();

        div()
            .key_context("RepoComposer")
            // A miss lands in the field anyway. There is one control on this
            // surface, so a click on the label, the margin or the empty half of
            // the row means "type here" and nothing else — and gpuikit raises
            // `Blur` from the input's *paint*, so a click that moves focus
            // nowhere raises no blur and cannot close the window on the way.
            .id("repo-composer")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    window.focus(&this.input.focus_handle(cx), cx);
                }),
            )
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT)
            .bg(theme.bg())
            .text_color(theme.fg())
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(6.))
                    .p(px(12.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child("Repository"),
                    )
                    // The box is this window's, not the field's: with one
                    // control on the surface and nothing else to click, an
                    // unframed line of text does not read as somewhere to type.
                    .child(
                        div()
                            .flex_none()
                            .px(px(8.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.input_border())
                            .bg(theme.input_bg())
                            .child(text_field(&self.input, "owner/repo", cx)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(theme.border_subtle())
                    .text_xs()
                    .text_color(theme.fg_muted())
                    // The bulk-intake invariant, said where somebody is about
                    // to add a repository with 11,000 open issues.
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child("Its issues land in the backlog — nothing is scouted or built until you queue it"),
                    )
                    .child(
                        div()
                            .id("add-repo")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .when(ready, |el| {
                                let hover_bg = theme.surface_secondary();
                                el.cursor_pointer()
                                    .text_color(theme.fg())
                                    .hover(move |el| el.bg(hover_bg))
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.submit(window, cx);
                                    }))
                            })
                            .when(!ready, |el| el.text_color(theme.fg_muted()))
                            .child("Add repo ⌘↩"),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_splits_into_the_two_request_fields() {
        assert_eq!(
            split_slug("iamnbutler/tasks"),
            Some(("iamnbutler".into(), "tasks".into()))
        );
    }

    /// The incidental punctuation a paste carries. The server normalizes the
    /// same way — this exists so the button is not dead while it is there.
    #[test]
    fn the_punctuation_a_paste_carries_is_tolerated() {
        for draft in [
            "  iamnbutler/tasks  ",
            "iamnbutler/tasks/",
            "/iamnbutler/tasks",
            "iamnbutler / tasks",
        ] {
            assert_eq!(
                split_slug(draft),
                Some(("iamnbutler".into(), "tasks".into())),
                "{draft}"
            );
        }
    }

    #[test]
    fn anything_that_is_not_owner_slash_repo_leaves_the_button_dead() {
        for draft in [
            "",
            "   ",
            "tasks",
            "iamnbutler/",
            "/tasks",
            "/",
            // A pasted URL is not a slug, and guessing at one here would be
            // the second parser this deliberately does not have.
            "https://github.com/iamnbutler/tasks",
        ] {
            assert_eq!(split_slug(draft), None, "{draft:?}");
        }
    }
}
