//! Dockable side panels.
//!
//! Modeled on Zed's workspace docks: the workspace owns each sidebar's
//! open/width state and registers the toggle actions; this component is
//! pure presentation plus a resize handle. The handle reports drag-start
//! through a callback — width updates during the drag are handled by the
//! workspace, which watches window-level mouse moves (a drag outruns the
//! handle's own hitbox immediately).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, MouseDownEvent, Pixels, Window};
use gpuikit::theme::{ActiveTheme, Themeable};
use smallvec::SmallVec;

pub const DEFAULT_SIDEBAR_WIDTH: Pixels = px(240.);
pub const MIN_SIDEBAR_WIDTH: Pixels = px(160.);

/// A sidebar may take up to this share of the window — reading surfaces
/// like the inspector's spec view need real width, so the ceiling is
/// proportional, not a fixed pixel count.
pub const MAX_SIDEBAR_FRACTION: f32 = 0.5;

const RESIZE_HANDLE_WIDTH: Pixels = px(5.);

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum SidebarSide {
    Left,
    Right,
}

/// Per-sidebar state owned by the workspace (Zed's `Dock` holds the same).
pub struct SidebarState {
    pub open: bool,
    pub width: Pixels,
}

impl SidebarState {
    pub fn new(open: bool) -> Self {
        Self {
            open,
            width: DEFAULT_SIDEBAR_WIDTH,
        }
    }

    pub fn with_width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    /// Clamp to the legal range for the current window: at least
    /// [`MIN_SIDEBAR_WIDTH`], at most [`MAX_SIDEBAR_FRACTION`] of it.
    pub fn set_width(&mut self, width: Pixels, viewport_width: Pixels) {
        let max = viewport_width * MAX_SIDEBAR_FRACTION;
        self.width = width.clamp(MIN_SIDEBAR_WIDTH, max.max(MIN_SIDEBAR_WIDTH));
    }
}

type ResizeStartHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct Sidebar {
    side: SidebarSide,
    width: Pixels,
    children: SmallVec<[AnyElement; 4]>,
    on_resize_start: Option<ResizeStartHandler>,
}

pub fn sidebar(side: SidebarSide, width: Pixels) -> Sidebar {
    Sidebar {
        side,
        width,
        children: SmallVec::new(),
        on_resize_start: None,
    }
}

impl Sidebar {
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Called when the user presses down on the resize handle. The workspace
    /// takes over tracking from there.
    pub fn on_resize_start(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_resize_start = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Sidebar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let side = self.side;

        let mut handle = div()
            .id(match side {
                SidebarSide::Left => "sidebar-resize-handle-left",
                SidebarSide::Right => "sidebar-resize-handle-right",
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(RESIZE_HANDLE_WIDTH)
            .cursor_col_resize()
            .hover({
                let handle_hover = theme.border_secondary();
                move |el| el.bg(handle_hover)
            });
        handle = match side {
            // The handle straddles the sidebar's inner edge.
            SidebarSide::Left => handle.right(-RESIZE_HANDLE_WIDTH / 2.),
            SidebarSide::Right => handle.left(-RESIZE_HANDLE_WIDTH / 2.),
        };
        if let Some(on_resize_start) = self.on_resize_start {
            handle = handle.on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                on_resize_start(event, window, cx);
            });
        }

        div()
            .relative()
            .flex()
            .flex_col()
            .w(self.width)
            .flex_none()
            .h_full()
            .overflow_hidden()
            .bg(theme.surface())
            .map(|el| match side {
                SidebarSide::Left => el.border_r_1(),
                SidebarSide::Right => el.border_l_1(),
            })
            .border_color(theme.border_subtle())
            .children(self.children)
            .child(handle)
    }
}
