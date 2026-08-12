//! The root workspace view.
//!
//! Follows Zed's workspace/dock split: the workspace owns UI state
//! (active section, per-sidebar open/width) and registers action handlers;
//! chrome components (`TitleBar`, `Sidebar`) are presentation-only and talk
//! back by dispatching actions, never by reaching into workspace state.

use gpui::prelude::*;
use gpui::{actions, div, px, Context, Div, Entity, Focusable, MouseButton, Window};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::text_area;
use gpuikit::input::InputState;
use gpuikit::theme::{ActiveTheme, Themeable};
use gpuikit::DefaultIcons as Icons;

use crate::components::{sidebar, title_bar, SidebarSide, SidebarState};

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
    section: Section,
    left_sidebar: SidebarState,
    right_sidebar: SidebarState,
    /// Which sidebar is currently being drag-resized, if any.
    resizing: Option<SidebarSide>,
    input: Entity<InputState>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut state = InputState::new_multiline(cx);
            state.set_content(
                "The composer input for chat / review notes lands here.\n\nType away — this is gpuikit's InputState + text_area on gpui 1.14.2.",
                cx,
            );
            state
        });
        window.focus(&input.focus_handle(cx), cx);

        Self {
            section: Section::Home,
            left_sidebar: SidebarState::new(true),
            right_sidebar: SidebarState::new(false),
            resizing: None,
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
            .child_right(Self::title_bar_button("settings", Icons::gear()).disabled(true))
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
        let (text, text_muted, selected_bg, hover_bg) = (
            theme.fg(),
            theme.fg_muted(),
            theme.surface_tertiary(),
            theme.surface_secondary(),
        );
        let active = self.section;

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
            .child(div().flex().flex_col().pt(px(8.)).children(
                Section::ALL.into_iter().enumerate().map(|(ix, section)| {
                    let selected = section == active;
                    div()
                        .id(ix)
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
                        .text_sm()
                        .text_color(if selected { text } else { text_muted })
                        .child(section.label())
                }),
            ))
    }

    fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (text, text_muted) = (theme.fg(), theme.fg_muted());

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
            .child(
                div()
                    .px(px(12.))
                    .py(px(10.))
                    .text_color(text)
                    .child("Inspector"),
            )
            .child(
                div()
                    .px(px(12.))
                    .text_sm()
                    .text_color(text_muted)
                    .child("Task detail / review pane placeholder."),
            )
    }

    fn render_center(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (text, editor_bg) = (theme.fg(), theme.bg());

        div()
            .flex()
            .flex_col()
            .flex_grow(1.)
            .h_full()
            .overflow_hidden()
            .bg(editor_bg)
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .text_color(text)
                    .child(self.section.label()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow(1.)
                    .overflow_hidden()
                    .p(px(16.))
                    .text_sm()
                    .text_color(text)
                    .child(text_area(&self.input, cx).size_full()),
            )
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
