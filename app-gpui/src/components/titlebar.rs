//! Per-pane window chrome.
//!
//! The v3 design has no full-width title bar: the left rail's header holds
//! the traffic lights, project switcher and pipeline controls, and the
//! middle column's header holds history and the tab strip. What survives
//! from the old bar is the *mechanics*, shared by every pane header —
//! following Zed's `PlatformTitleBar`: the row is a
//! `WindowControlArea::Drag` region, double-click zooms via
//! `window.titlebar_double_click()`, and whichever header sits leftmost in
//! the window asks for `.traffic_light_inset()` to clear the macOS lights
//! (dropped automatically in fullscreen, where the lights hide).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ElementId, Pixels, Window, WindowControlArea};
use smallvec::SmallVec;

pub const PANE_HEADER_HEIGHT: Pixels = px(28.);

/// Left inset that clears the macOS traffic lights. Per the design spec:
/// 8px inset + 52px light group + 8px gap before content.
pub const TRAFFIC_LIGHT_PADDING: Pixels = px(68.);

#[derive(IntoElement)]
pub struct PaneHeader {
    id: ElementId,
    inset: bool,
    children: SmallVec<[AnyElement; 6]>,
}

/// A pane's top row: fixed-height, a window-drag region, double-click to
/// zoom. Content is the caller's; so is any background or border, since the
/// header should read as part of its pane, not as a bar laid over it.
pub fn pane_header(id: impl Into<ElementId>) -> PaneHeader {
    PaneHeader {
        id: id.into(),
        inset: false,
        children: SmallVec::new(),
    }
}

impl PaneHeader {
    /// Clear the macOS traffic lights. For whichever header is leftmost —
    /// the rail's normally, the middle column's when the rail is hidden.
    pub fn traffic_light_inset(mut self) -> Self {
        self.inset = true;
        self
    }

    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }
}

impl RenderOnce for PaneHeader {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .id(self.id)
            .window_control_area(WindowControlArea::Drag)
            .on_click(|event, window, _cx| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(PANE_HEADER_HEIGHT)
            .flex_none()
            .map(|this| {
                if self.inset && !window.is_fullscreen() {
                    this.pl(TRAFFIC_LIGHT_PADDING)
                } else {
                    this.pl(px(8.))
                }
            })
            .pr(px(8.))
            .children(self.children)
    }
}
