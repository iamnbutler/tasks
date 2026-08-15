//! The cmd-n "new issue" window.
//!
//! A small separate OS window (Zed's new-window flow: `cx.open_window` off a
//! spawn, root view built in the window's context), not an in-window overlay.
//! One field, no chrome of its own beyond the system title bar. Cmd-enter
//! hands the draft to the orchestrator — which owns picking the repo, titling
//! the issue, and folding in its ambient context — and closes the window.
//!
//! The draft `InputState` is owned by the workspace, so escape or closing
//! the window keeps the text; the next cmd-n picks it back up.

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
            title: Some("New Issue".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(520.), px(240.)),
            cx,
        ))),
        ..Default::default()
    }
}

pub struct IssueComposer {
    /// Fallback send path if the main workspace is somehow gone.
    app_state: Entity<AppState>,
    input: Entity<InputState>,
    workspace: WeakEntity<Workspace>,
}

impl IssueComposer {
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
                // nothing else takes focus — a blur is escape. The draft
                // stays in the workspace-owned input.
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
        self.input.update(cx, |input, cx| input.set_content("", cx));
        let message = format!(
            "Create a new GitHub issue from the draft below. Pick the right \
             repository, write a clear, specific title, and expand the body with \
             any relevant context you have (related tasks, recent activity, code \
             areas). File it and reply with the issue number and link.\n\n\
             Draft:\n{draft}"
        );
        // Route through the workspace so the main window jumps to Chat —
        // that's where the orchestrator's reply lands once this closes.
        if self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.ask_orchestrator(message.clone(), cx)
            })
            .is_err()
        {
            self.app_state
                .update(cx, |state, cx| state.send_orchestrator_message(message, cx));
        }
        window.remove_window();
    }
}

impl Render for IssueComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .key_context("IssueComposer")
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
                    .child("The orchestrator picks the repo and files it")
                    .child(
                        div()
                            .id("file-issue")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .cursor_pointer()
                            .text_color(theme.fg())
                            .hover({
                                let hover_bg = theme.surface_secondary();
                                move |el| el.bg(hover_bg)
                            })
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.submit(window, cx);
                            }))
                            .child("File issue ⌘↩"),
                    ),
            )
    }
}
