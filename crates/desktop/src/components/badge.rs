//! Badge component with color variants
//!
//! Mirrors the shadcn/ui Badge component for displaying status and labels.

use gpui::{div, px, Div, Hsla, IntoElement, ParentElement, SharedString, Styled};

use crate::theme::Theme;

/// Badge style variants
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum BadgeVariant {
    #[default]
    Default,
    Secondary,
    Destructive,
    Outline,
    Ghost,
}

/// A badge component for displaying status labels
pub struct Badge {
    label: SharedString,
    variant: BadgeVariant,
}

impl Badge {
    /// Create a new badge with the given label
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            variant: BadgeVariant::default(),
        }
    }

    /// Set the badge variant
    pub fn variant(mut self, variant: BadgeVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Convenience method for default variant
    pub fn default_variant(self) -> Self {
        self.variant(BadgeVariant::Default)
    }

    /// Convenience method for secondary variant
    pub fn secondary(self) -> Self {
        self.variant(BadgeVariant::Secondary)
    }

    /// Convenience method for destructive variant
    pub fn destructive(self) -> Self {
        self.variant(BadgeVariant::Destructive)
    }

    /// Convenience method for outline variant
    pub fn outline(self) -> Self {
        self.variant(BadgeVariant::Outline)
    }

    /// Convenience method for ghost variant
    pub fn ghost(self) -> Self {
        self.variant(BadgeVariant::Ghost)
    }

    fn get_colors(&self, theme: &Theme) -> (Hsla, Hsla, Option<Hsla>) {
        // Returns (background, text_color, border_color)
        match self.variant {
            BadgeVariant::Default => (theme.primary, theme.primary_foreground, None),
            BadgeVariant::Secondary => (theme.secondary, theme.secondary_foreground, None),
            BadgeVariant::Destructive => (theme.destructive, theme.destructive_foreground, None),
            BadgeVariant::Outline => (
                gpui::hsla(0.0, 0.0, 0.0, 0.0),
                theme.foreground,
                Some(theme.border),
            ),
            BadgeVariant::Ghost => (gpui::hsla(0.0, 0.0, 0.0, 0.0), theme.foreground, None),
        }
    }

    /// Render the badge into a GPUI element
    pub fn render(self) -> Div {
        let theme = Theme::default();
        let (bg_color, text_color, border_color) = self.get_colors(&theme);

        let mut base = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_1()
            .h(px(20.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .rounded(px(9999.0)) // pill shape
            .bg(bg_color)
            .text_color(text_color)
            .overflow_hidden()
            .whitespace_nowrap();

        // Apply border for outline variant
        if let Some(border) = border_color {
            base = base.border_1().border_color(border);
        }

        base.child(self.label.clone())
    }
}

impl IntoElement for Badge {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}
