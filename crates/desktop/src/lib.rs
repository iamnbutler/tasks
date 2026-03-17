//! Tasks Desktop — GPUI-based desktop application.
//!
//! This crate provides a native desktop UI for the Tasks platform,
//! built with GPUI (Zed's GPU-accelerated UI framework).
//!
//! # Modules
//!
//! - [`components`] - Reusable UI primitive components (Button, Badge, Card, Input)
//! - [`sse`] - Server-Sent Events client for real-time updates
//! - [`theme`] - Theming and styling system

use gpui::{Context, SharedString, Window, div, prelude::*, rgb};

pub mod components;
mod sse;
pub mod theme;

pub use sse::{SseClient, SseClientEvent, SseConnectionState, SseFilters};
pub use theme::{Theme, colors, radius, spacing, style_helpers, typography};

/// Root view for the Tasks desktop application.
pub struct RootView {
    title: SharedString,
}

impl RootView {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .justify_center()
            .items_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .items_center()
                    .child(
                        div()
                            .text_3xl()
                            .text_color(rgb(0xcdd6f4))
                            .child(self.title.clone()),
                    )
                    .child(div().text_color(rgb(0xa6adc8)).child("GPUI Desktop Shell")),
            )
    }
}
