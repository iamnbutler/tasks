//! The window title bar.
//!
//! Follows Zed's `PlatformTitleBar` (crates/platform_title_bar): the whole
//! bar is a `WindowControlArea::Drag` region, double-click zooms via
//! `window.titlebar_double_click()`, and content is inset past the macOS
//! traffic lights unless the window is fullscreen. Measurements come from
//! the design spec: 28px fixed height, 1px bottom border.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, Pixels, Window, WindowControlArea};
use gpuikit::theme::{ActiveTheme, Themeable};
use smallvec::SmallVec;

pub const TITLE_BAR_HEIGHT: Pixels = px(28.);

/// Left inset that clears the macOS traffic lights. Per the design spec:
/// 8px inset + 52px light group + 8px gap before content.
pub const TRAFFIC_LIGHT_PADDING: Pixels = px(68.);

#[derive(IntoElement)]
pub struct TitleBar {
    left_children: SmallVec<[AnyElement; 4]>,
    center_children: SmallVec<[AnyElement; 2]>,
    right_children: SmallVec<[AnyElement; 4]>,
}

pub fn title_bar() -> TitleBar {
    TitleBar {
        left_children: SmallVec::new(),
        center_children: SmallVec::new(),
        right_children: SmallVec::new(),
    }
}

impl TitleBar {
    /// Content placed just right of the traffic lights.
    pub fn child_left(mut self, child: impl IntoElement) -> Self {
        self.left_children.push(child.into_any_element());
        self
    }

    /// Content centered in the bar. Empty since the v3 frame swap (the tab
    /// strip names the view now); kept as chrome API — the slot is real even
    /// while nothing occupies it.
    #[allow(dead_code)]
    pub fn child_center(mut self, child: impl IntoElement) -> Self {
        self.center_children.push(child.into_any_element());
        self
    }

    /// Content aligned to the right edge.
    pub fn child_right(mut self, child: impl IntoElement) -> Self {
        self.right_children.push(child.into_any_element());
        self
    }
}

impl RenderOnce for TitleBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .on_click(|event, window, _cx| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .h(TITLE_BAR_HEIGHT)
            .flex_none()
            .bg(theme.surface())
            .border_b_1()
            .border_color(theme.border_subtle())
            .map(|this| {
                if window.is_fullscreen() {
                    this.pl_2()
                } else {
                    this.pl(TRAFFIC_LIGHT_PADDING)
                }
            })
            .pr(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .children(self.left_children),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .children(self.center_children),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .children(self.right_children),
            )
    }
}
