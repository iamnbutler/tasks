//! Input component for text entry
//!
//! Mirrors the shadcn/ui Input component for text input with focus states.
//! Note: This is a basic visual representation. Full text input functionality
//! requires more complex state management with GPUI's focus system.

use gpui::{div, px, Div, IntoElement, ParentElement, SharedString, Styled};

use crate::theme::Theme;

/// A text input component (visual only for now)
pub struct Input {
    value: SharedString,
    placeholder: SharedString,
    disabled: bool,
}

impl Input {
    /// Create a new input
    pub fn new() -> Self {
        Self {
            value: SharedString::default(),
            placeholder: SharedString::default(),
            disabled: false,
        }
    }

    /// Set the input value
    pub fn value(mut self, value: impl Into<SharedString>) -> Self {
        self.value = value.into();
        self
    }

    /// Set the placeholder text
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Set the disabled state
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Render the input into a GPUI element
    pub fn render(self) -> Div {
        let theme = Theme::default();

        let mut base = div()
            .flex()
            .items_center()
            .h(px(36.0))
            .w_full()
            .min_w_0()
            .rounded(theme.radius_md)
            .border_1()
            .border_color(theme.input)
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.0))
            .px(px(12.0))
            .py(px(4.0))
            .text_size(px(14.0))
            .text_color(theme.foreground);

        // Apply disabled styles
        if self.disabled {
            base = base.opacity(0.5).cursor_not_allowed();
        } else {
            base = base.cursor_text();
        }

        // Render value or placeholder
        if self.value.is_empty() {
            base.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(self.placeholder.clone()),
            )
        } else {
            base.child(self.value.clone())
        }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for Input {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}
