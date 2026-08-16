//! The cmd-n "new issue" window.
//!
//! A small separate OS window (Zed's new-window flow: `cx.open_window` off a
//! spawn, root view built in the window's context), not an in-window overlay.
//! One field, no chrome of its own beyond the system title bar. Cmd-enter
//! hands the draft to the orchestrator — which owns titling the issue and
//! folding in its ambient context — and closes the window.
//!
//! **The repo is the app's to name, not the orchestrator's to pick.** The
//! server already refuses to guess between several projects; this window states
//! which one it will file into, carries the `project_id` verbatim in the
//! message so the agent copies a value rather than re-deriving one, and refuses
//! to send when several repos are in view and none is selected. Selecting one
//! in the title bar's switcher is the answer.
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

use crate::projects::{self, IssueTarget, ProjectFilter};
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

    /// Which repo this window would file into, read out of the workspace's
    /// switcher — or out of the app state alone if the workspace is gone.
    fn target(&self, cx: &App) -> IssueTarget {
        match self.workspace.upgrade() {
            Some(workspace) => {
                let workspace = workspace.read(cx);
                let state = workspace.app_state.read(cx);
                projects::issue_target(&state.projects, &workspace.project_filter)
            }
            None => projects::issue_target(&self.app_state.read(cx).projects, &ProjectFilter::All),
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).content().trim().to_string();
        if draft.is_empty() {
            return;
        }
        // The one case this refuses: the server refuses to guess between
        // several projects, so asking the orchestrator to pick would only move
        // the guess somewhere less accountable.
        let (project_id, slug) = match self.target(cx) {
            IssueTarget::Repo { id, slug } => (id, slug),
            IssueTarget::NoProjects => {
                self.app_state.update(cx, |state, cx| {
                    state.report("no repository to file into — add one first", cx)
                });
                return;
            }
            IssueTarget::Ambiguous { count } => {
                self.app_state.update(cx, |state, cx| {
                    state.report(
                        format!("{count} repos in view — pick one in the title bar first"),
                        cx,
                    )
                });
                return;
            }
        };
        self.input.update(cx, |input, cx| input.set_content("", cx));
        let message = format!(
            "Create a new GitHub issue from the draft below, in {slug}. Pass \
             \"project_id\": \"{project_id}\" to POST /issues — that repository is \
             chosen, not for you to re-derive. Write a clear, specific title, and \
             expand the body with any relevant context you have (related tasks, \
             recent activity, code areas). File it and reply with the issue \
             number and link.\n\n\
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
        let (footer, can_send) = match self.target(cx) {
            IssueTarget::Repo { slug, .. } => (format!("Files into {slug}"), true),
            IssueTarget::NoProjects => ("No repository to file into".to_string(), false),
            IssueTarget::Ambiguous { count } => (
                format!("{count} repos in view — pick one in the title bar"),
                false,
            ),
        };
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
                    .child(footer)
                    .child(
                        div()
                            .id("file-issue")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .map(|el| {
                                if can_send {
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
                            .child("File issue ⌘↩"),
                    ),
            )
    }
}
