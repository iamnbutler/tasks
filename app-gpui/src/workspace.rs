//! The root workspace view.
//!
//! Follows Zed's workspace/dock split: the workspace owns UI state
//! (active section, per-sidebar open/width, selection) and registers action
//! handlers; chrome components (`TitleBar`, `Sidebar`) are presentation-only
//! and talk back by dispatching actions, never by reaching into workspace
//! state. Server state lives in [`AppState`]; the workspace observes it and
//! re-renders.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    actions, div, list, px, Context, Div, Entity, Focusable, ListAlignment, ListState, MouseButton,
    Window,
};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::text_area;
use gpuikit::input::{InputState, InputStateEvent, SubmitOn};
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{
    BuildStatus, Mode, SessionStatus, SpecQueueStatus, TaskId, TaskState,
};

use crate::components::{sidebar, title_bar, SidebarSide, SidebarState};
use crate::state::AppState;

pub(crate) const FONT: &str = "Menlo";

actions!(workspace, [ToggleLeftDock, ToggleRightDock]);

#[derive(PartialEq, Clone, Copy)]
pub enum Section {
    Home,
    Tasks,
    Queue,
    Activity,
    Chat,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Home,
        Section::Tasks,
        Section::Queue,
        Section::Activity,
        Section::Chat,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Home => "Home",
            Section::Tasks => "Tasks",
            Section::Queue => "Queue",
            Section::Activity => "Activity",
            Section::Chat => "Chat",
        }
    }
}

pub struct Workspace {
    pub(crate) section: Section,
    pub(crate) left_sidebar: SidebarState,
    pub(crate) right_sidebar: SidebarState,
    /// Which sidebar is currently being drag-resized, if any.
    pub(crate) resizing: Option<SidebarSide>,
    pub(crate) app_state: Entity<AppState>,
    /// Task shown in the inspector (right sidebar).
    pub(crate) selected_task: Option<TaskId>,
    /// Chat composer.
    pub(crate) input: Entity<InputState>,
    /// Review-form composer in the inspector — feedback for a re-scout or a
    /// question for the orchestrator, depending on which button submits it.
    pub(crate) review_input: Entity<InputState>,
    /// Scroll state for the chat list — bottom-aligned, so the view opens
    /// at the newest message and follows new ones.
    chat_list: ListState,
    /// Message count the list was last synced to.
    chat_len: usize,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app_state = cx.new(AppState::new);
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();

        // Live elapsed clocks (running scouts/builds) tick once a second —
        // but only when something is actually running.
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            executor.timer(Duration::from_secs(1)).await;
            let alive = this
                .update(cx, |this: &mut Workspace, cx| {
                    let state = this.app_state.read(cx);
                    let live = state
                        .sessions
                        .iter()
                        .any(|session| session.status == SessionStatus::Running)
                        || state
                            .builds
                            .iter()
                            .any(|build| build.status == BuildStatus::Running);
                    if live {
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                return;
            }
        })
        .detach();

        let input = cx.new(|cx| {
            // Cmd-enter sends everywhere in this app; enter is a newline.
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Talk to the orchestrator…", cx);
            state
        });
        let review_input = cx.new(|cx| {
            // Compose convention: cmd-enter fires the primary action (Ask),
            // plain enter stays a newline — feedback is often multi-line.
            let mut state = InputState::new_multiline(cx).submit_on(SubmitOn::CmdEnter);
            state.set_placeholder("Feedback or a question about this spec…", cx);
            state
        });
        // The composers gate their submit buttons on content, so keystrokes
        // must re-render the workspace, not just the input element.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        cx.observe(&review_input, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.send_chat(cx);
            }
        })
        .detach();
        cx.subscribe(&review_input, |this, _, event: &InputStateEvent, cx| {
            if matches!(event, InputStateEvent::Submit) {
                this.ask_about_selected_spec(cx);
            }
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);

        Self {
            section: Section::Home,
            left_sidebar: SidebarState::new(true),
            // The inspector is a reading surface (specs, task bodies) —
            // default it wide, like the Swift app's 460pt ideal.
            right_sidebar: SidebarState::new(false).with_width(px(460.)),
            resizing: None,
            app_state,
            selected_task: None,
            input,
            review_input,
            chat_list: ListState::new(0, ListAlignment::Bottom, px(1024.)),
            chat_len: 0,
        }
    }

    fn sidebar_mut(&mut self, side: SidebarSide) -> &mut SidebarState {
        match side {
            SidebarSide::Left => &mut self.left_sidebar,
            SidebarSide::Right => &mut self.right_sidebar,
        }
    }

    fn toggle_sidebar(&mut self, side: SidebarSide, cx: &mut Context<Self>) {
        let state = self.sidebar_mut(side);
        state.open = !state.open;
        cx.notify();
    }

    // --- selection (called from section rows) ---

    pub(crate) fn select_task(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self.selected_task.as_ref() != Some(&id) {
            // Draft feedback is about one spec — don't carry it to another.
            self.review_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        self.selected_task = Some(id);
        self.right_sidebar.open = true;
        cx.notify();
    }

    /// Send a message into the orchestrator conversation and jump to Chat so
    /// the reply is visible as it streams in.
    pub(crate) fn ask_orchestrator(&mut self, message: String, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.send_orchestrator_message(message, cx));
        self.section = Section::Chat;
        cx.notify();
    }

    /// Submit the chat composer, if it has content.
    pub(crate) fn send_chat(&mut self, cx: &mut Context<Self>) {
        let content = self.input.read(cx).content().trim().to_string();
        if content.is_empty() {
            return;
        }
        self.input.update(cx, |input, cx| input.set_content("", cx));
        self.app_state
            .update(cx, |state, cx| state.send_orchestrator_message(content, cx));
    }

    /// Submit the review draft as a question about the selected task's
    /// pending spec — the review form's primary (cmd-enter) action. No-op
    /// without a selected task, a pending spec, or draft text.
    pub(crate) fn ask_about_selected_spec(&mut self, cx: &mut Context<Self>) {
        let Some((number, title, spec_id)) = ({
            let state = self.app_state.read(cx);
            self.selected_task
                .as_ref()
                .and_then(|id| state.task(id))
                .and_then(|task| {
                    let spec = state.latest_spec(&task.id)?;
                    state
                        .spec_queue
                        .iter()
                        .any(|item| {
                            item.entry.spec_id == spec.id
                                && item.entry.status == SpecQueueStatus::PendingReview
                        })
                        .then(|| (task.gh_issue_number, task.title.clone(), spec.id.clone()))
                })
        }) else {
            return;
        };
        let Some(text) = self.take_review_draft(cx) else {
            return;
        };
        let message = format!(
            "Re: task #{number} \"{title}\" — its spec ({spec_id}) is pending review.\n\n{text}"
        );
        self.ask_orchestrator(message, cx);
    }

    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_task = None;
        self.right_sidebar.open = false;
        cx.notify();
    }

    // Chrome

    /// A title-bar icon button at the design spec's metrics: 14px icon with
    /// 8px horizontal / 7px vertical padding, so the button fills the bar.
    fn title_bar_button(
        id: &'static str,
        icon: gpui::Svg,
    ) -> gpuikit::elements::icon_button::IconButton {
        icon_button(id, icon)
            .width(px(30.))
            .height(px(28.))
            .icon_size(px(14.))
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.app_state.read(cx).mode;

        title_bar()
            .child_left(
                Self::title_bar_button("toggle-left-sidebar", Icons::panel_left())
                    .selected(self.left_sidebar.open)
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleLeftDock), cx);
                    }),
            )
            .child_left(
                Self::title_bar_button("open-chat", Icons::chat_bubble()).on_click(cx.listener(
                    |this, _event, _window, cx| {
                        this.section = Section::Chat;
                        cx.notify();
                    },
                )),
            )
            .child_center(div().child("tasks"))
            .child_right(
                Self::title_bar_button("mode-play", Icons::play())
                    .selected(mode == Some(Mode::Play))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.set_mode(Mode::Play, cx));
                    })),
            )
            .child_right(
                Self::title_bar_button("mode-pause", Icons::pause())
                    .selected(mode == Some(Mode::Pause))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.set_mode(Mode::Pause, cx));
                    })),
            )
            .child_right(
                Self::title_bar_button("refresh", Icons::reload()).on_click(cx.listener(
                    |this, _event, _window, cx| {
                        this.app_state.update(cx, |state, cx| state.refresh(cx));
                    },
                )),
            )
            .child_right(
                Self::title_bar_button("toggle-right-sidebar", Icons::panel_right())
                    .selected(self.right_sidebar.open)
                    .on_click(|_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleRightDock), cx);
                    }),
            )
    }

    fn render_left_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (text, text_muted, selected_bg, hover_bg, badge_bg) = (
            theme.fg(),
            theme.fg_muted(),
            theme.surface_tertiary(),
            theme.surface_secondary(),
            theme.surface_tertiary(),
        );
        let active = self.section;

        let state = self.app_state.read(cx);
        let queued_work = state
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.state,
                    TaskState::Queued
                        | TaskState::Scouting
                        | TaskState::InReview
                        | TaskState::ReadyToBuild
                        | TaskState::Building
                )
            })
            .count();
        let banner = if let Some(error) = &state.error {
            Some((error.clone(), true))
        } else if state.loaded && !state.connected {
            Some(("Reconnecting to the tasks server…".to_string(), false))
        } else {
            None
        };

        sidebar(SidebarSide::Left, self.left_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Left);
                            cx.notify();
                        });
                    }
                }
            })
            .child(div().flex().flex_col().flex_1().pt(px(8.)).children(
                Section::ALL.into_iter().enumerate().map(|(ix, section)| {
                    let selected = section == active;
                    let badge = (section == Section::Queue && queued_work > 0)
                        .then(|| queued_work.to_string());
                    div()
                        .id(ix)
                        .flex()
                        .flex_row()
                        .items_center()
                        .mx(px(6.))
                        .px(px(10.))
                        .py(px(5.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .when(!selected, |el| el.hover(move |el| el.bg(hover_bg)))
                        .when(selected, |el| el.bg(selected_bg))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.section = section;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(if selected { text } else { text_muted })
                                .child(section.label()),
                        )
                        .when_some(badge, |el, badge| {
                            el.child(
                                div()
                                    .flex_none()
                                    .px(px(6.))
                                    .rounded_full()
                                    .bg(badge_bg)
                                    .text_xs()
                                    .text_color(text_muted)
                                    .child(badge),
                            )
                        })
                }),
            ))
            .child(div().flex_1())
            .when_some(banner, |el, (message, is_error)| {
                el.child(
                    div()
                        .m(px(6.))
                        .p(px(8.))
                        .rounded(px(5.))
                        .bg(cx.theme().surface_secondary())
                        .text_xs()
                        .text_color(if is_error {
                            gpui::hsla(30. / 360., 0.9, 0.6, 1.)
                        } else {
                            cx.theme().fg_muted()
                        })
                        .child(message),
                )
            })
    }

    fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let inspector = self.render_inspector(cx);
        sidebar(SidebarSide::Right, self.right_sidebar.width)
            .on_resize_start({
                let entity = cx.entity().downgrade();
                move |_event, _window, cx| {
                    if let Some(workspace) = entity.upgrade() {
                        workspace.update(cx, |this, cx| {
                            this.resizing = Some(SidebarSide::Right);
                            cx.notify();
                        });
                    }
                }
            })
            .child(inspector)
    }

    fn render_chat(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // Zed's agent-panel pattern: a bottom-aligned `list` starts at the
        // newest message and stays pinned there as new ones land; item
        // count is synced in `Render::render` via splice.
        let app_state = self.app_state.clone();
        let messages = list(self.chat_list.clone(), move |ix, _window, cx| {
            use tasks_client::api::models::ChatRole;
            let theme = cx.theme().clone();
            let state = app_state.read(cx);
            let Some(message) = state.orchestrator_messages.get(ix) else {
                return div().into_any_element();
            };
            let (role, content) = (message.role, message.content.clone());

            let bubble = div()
                .max_w(px(720.))
                .p(px(8.))
                .rounded(px(8.))
                .text_sm()
                .child(content);
            div()
                .w_full()
                .px(px(12.))
                .py(px(4.))
                .flex()
                .flex_row()
                .map(|el| match role {
                    ChatRole::User => el
                        .justify_end()
                        .child(bubble.bg(theme.accent_bg()).text_color(theme.fg())),
                    ChatRole::Assistant => {
                        el.child(bubble.bg(theme.surface_secondary()).text_color(theme.fg()))
                    }
                    ChatRole::Event => el.child(bubble.text_color(theme.fg_muted()).text_xs()),
                    // A session seam. The conversation reads as continuous
                    // here but the orchestrator's memory does not, so it is
                    // centered like a divider rather than sitting in the
                    // flow of turns.
                    ChatRole::System => el
                        .justify_center()
                        .child(bubble.text_color(theme.fg_muted()).text_xs()),
                })
                .into_any_element()
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .map(|el| {
                if self.chat_len == 0 {
                    el.child(
                        div()
                            .flex_1()
                            .p(px(16.))
                            .text_sm()
                            .text_color(theme.fg_muted())
                            .child("Talk to the orchestrator — the conversation lands here."),
                    )
                } else {
                    el.child(messages.flex_1().min_h(px(0.)).w_full().py(px(8.)))
                }
            })
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .border_t_1()
                    .border_color(theme.border_subtle())
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(8.))
                    .text_sm()
                    .child(
                        // The multiline input fills its parent, so the parent
                        // must own a height — unsized, it collapses to zero.
                        div()
                            .flex_1()
                            .h(px(64.))
                            .child(text_area(&self.input, cx).size_full()),
                    )
                    .child(
                        div()
                            .id("chat-send")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme.fg())
                            .hover({
                                let hover_bg = theme.surface_secondary();
                                move |el| el.bg(hover_bg)
                            })
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.send_chat(cx);
                            }))
                            .child("Send"),
                    ),
            )
    }

    fn render_center(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let loaded = self.app_state.read(cx).loaded;

        let mut pane = div()
            .flex()
            .flex_col()
            .flex_grow(1.)
            .h_full()
            .overflow_hidden()
            .bg(theme.bg());

        if !loaded {
            return pane.child(
                div()
                    .p(px(16.))
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child("Connecting to the tasks server…"),
            );
        }

        // Chat is a full-height conversation — the sidebar already names it,
        // so it skips the header rather than spending a row on the word
        // "Chat".
        if self.section != Section::Chat {
            pane = pane.child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .py(px(10.))
                    .text_color(theme.fg())
                    .child(self.section.label()),
            );
        }

        // The body must be a shrinkable flex child (`flex_1` + `min_h(0)`),
        // never `size_full`: 100% of the pane plus the header above it
        // overflows the clip and cuts off the bottom (chat's composer).
        let body = match self.section {
            Section::Home => self.render_home(cx).into_any_element(),
            Section::Tasks => self.render_tasks(cx).into_any_element(),
            Section::Queue => self.render_queue(cx).into_any_element(),
            Section::Activity => self.render_activity(cx).into_any_element(),
            Section::Chat => self.render_chat(cx).into_any_element(),
        };
        pane.child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport_width = window.viewport_size().width;

        // Re-clamp on every frame so a shrunk window can't leave a sidebar
        // owning more than its share.
        let left_width = self.left_sidebar.width;
        self.left_sidebar.set_width(left_width, viewport_width);
        let right_width = self.right_sidebar.width;
        self.right_sidebar.set_width(right_width, viewport_width);

        // Sync the chat list's item count with the message log (append-only,
        // so a shrink means the server was reset — start over).
        let messages_len = self.app_state.read(cx).orchestrator_messages.len();
        if messages_len != self.chat_len {
            if messages_len < self.chat_len {
                self.chat_list.reset(messages_len);
            } else {
                self.chat_list
                    .splice(self.chat_len..self.chat_len, messages_len - self.chat_len);
            }
            self.chat_len = messages_len;
        }

        div()
            .key_context("Workspace")
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT)
            // Themed default so nothing bottoms out at gpui's black.
            .text_color(cx.theme().fg())
            .on_action(cx.listener(|this, _: &ToggleLeftDock, _window, cx| {
                this.toggle_sidebar(SidebarSide::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, _window, cx| {
                this.toggle_sidebar(SidebarSide::Right, cx);
            }))
            // Drag-resize tracking: the handle only starts the drag; from
            // then on the pointer outruns it, so movement is tracked here at
            // the workspace root (which spans the window).
            .when(self.resizing.is_some(), |el| {
                el.cursor_col_resize().on_mouse_move(cx.listener(
                    move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                        if let Some(side) = this.resizing {
                            let width = match side {
                                SidebarSide::Left => event.position.x,
                                SidebarSide::Right => viewport_width - event.position.x,
                            };
                            this.sidebar_mut(side).set_width(width, viewport_width);
                            cx.notify();
                        }
                    },
                ))
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    if this.resizing.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow(1.)
                    .overflow_hidden()
                    .when(self.left_sidebar.open, |el| {
                        el.child(self.render_left_sidebar(cx))
                    })
                    .child(self.render_center(cx))
                    .when(self.right_sidebar.open, |el| {
                        el.child(self.render_right_sidebar(cx))
                    }),
            )
    }
}
