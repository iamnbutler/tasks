//! The cmd-n "new issue" window.
//!
//! A small separate OS window (Zed's new-window flow: `cx.open_window` off a
//! spawn, root view built in the window's context), not an in-window overlay.
//! One field, no chrome of its own beyond the system title bar. Cmd-enter
//! hands the draft to the orchestrator — which owns titling the issue and
//! folding in its ambient context — and closes the window.
//!
//! **The repo is the window's to decide, not the orchestrator's.** The server
//! already refuses to guess between several projects, so this stops asking an
//! agent to: the footer names the target, the send is dead while several repos
//! are in view and none is selected, and the message carries the `project_id`
//! verbatim so the agent copies a value rather than re-deriving one. See
//! [`crate::projects::issue_target`].
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

use crate::projects::{self, IssueTarget};
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

    /// Where this window would file, read out of the workspace's repo filter.
    ///
    /// Falls back to the app state directly if the workspace is gone, which is
    /// the same fallback `submit` uses to send.
    fn target(&self, cx: &App) -> IssueTarget {
        let filter = self
            .workspace
            .read_with(cx, |workspace, _| workspace.project_filter.clone())
            .unwrap_or_default();
        projects::issue_target(&self.app_state.read(cx).projects, &filter)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).content().trim().to_string();
        if draft.is_empty() {
            return;
        }
        // The refusal the server would make, made before the message is sent
        // rather than after: an ambiguous target reaching the orchestrator is
        // exactly the guess this window exists to stop asking for. The
        // message itself is shared with the rail composer — one flow, two
        // doors.
        let Some(message) = projects::issue_prompt(&self.target(cx), &draft) else {
            return;
        };
        self.input.update(cx, |input, cx| input.set_content("", cx));
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
        let target = self.target(cx);
        let can_file = target.can_file();
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
                    .gap(px(8.))
                    .text_xs()
                    .text_color(theme.fg_muted())
                    // Which repo, in words. The one thing this window used to
                    // leave to an agent.
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .truncate()
                            .child(target.sentence()),
                    )
                    .child(
                        div()
                            .id("file-issue")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .when(can_file, |el| {
                                let hover_bg = theme.surface_secondary();
                                el.cursor_pointer()
                                    .text_color(theme.fg())
                                    .hover(move |el| el.bg(hover_bg))
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.submit(window, cx);
                                    }))
                            })
                            .when(!can_file, |el| el.text_color(theme.fg_muted()).opacity(0.5))
                            .child("File issue ⌘↩"),
                    ),
            )
    }
}
