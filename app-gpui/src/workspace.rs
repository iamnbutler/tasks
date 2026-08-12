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
use gpui::{actions, div, px, Context, Div, Entity, Focusable, MouseButton, Window};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::text_area;
use gpuikit::input::InputState;
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;
use tasks_client::api::models::{BuildStatus, Mode, SessionStatus, TaskId, TaskState};

use crate::components::{sidebar, title_bar, SidebarSide, SidebarState};
use crate::state::AppState;

const FONT: &str = "Menlo";

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
    /// Chat composer (placeholder until the chat slice lands).
    pub(crate) input: Entity<InputState>,
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
            let mut state = InputState::new_multiline(cx);
            state.set_content("Talk to the orchestrator…", cx);
            state
        });
        window.focus(&input.focus_handle(cx), cx);

        Self {
            section: Section::Home,
            left_sidebar: SidebarState::new(true),
            right_sidebar: SidebarState::new(false),
            resizing: None,
            app_state,
            selected_task: None,
            input,
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
        self.selected_task = Some(id);
        self.right_sidebar.open = true;
        cx.notify();
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
        let theme = cx.theme();
        let state = self.app_state.read(cx);
        let messages: Vec<_> = state
            .orchestrator_messages
            .iter()
            .map(|message| (message.seq, message.role, message.content.clone()))
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .id("chat-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .p(px(12.))
                    .gap(px(8.))
                    .children(messages.into_iter().map(|(seq, role, content)| {
                        use tasks_client::api::models::ChatRole;
                        let bubble = div()
                            .max_w(px(720.))
                            .p(px(8.))
                            .rounded(px(8.))
                            .text_sm()
                            .child(content);
                        div()
                            .id(seq as usize)
                            .flex()
                            .flex_row()
                            .map(|el| match role {
                                ChatRole::User => el.justify_end().child(
                                    bubble
                                        .bg(cx.theme().accent_bg())
                                        .text_color(cx.theme().fg()),
                                ),
                                ChatRole::Assistant => el.child(
                                    bubble
                                        .bg(cx.theme().surface_secondary())
                                        .text_color(cx.theme().fg()),
                                ),
                                ChatRole::Event => {
                                    el.child(bubble.text_color(cx.theme().fg_muted()).text_xs())
                                }
                            })
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .border_t_1()
                    .border_color(theme.border_subtle())
                    .text_sm()
                    .child(text_area(&self.input, cx).size_full()),
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

        pane = pane.child(
            div()
                .flex_none()
                .px(px(16.))
                .py(px(10.))
                .text_color(theme.fg())
                .child(self.section.label()),
        );

        match self.section {
            Section::Home => pane.child(self.render_home(cx)),
            Section::Tasks => pane.child(self.render_tasks(cx)),
            Section::Queue => pane.child(self.render_queue(cx)),
            Section::Activity => pane.child(self.render_activity(cx)),
            Section::Chat => pane.child(self.render_chat(cx)),
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport_width = window.viewport_size().width;

        div()
            .key_context("Workspace")
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT)
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
                            this.sidebar_mut(side).set_width(width);
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
