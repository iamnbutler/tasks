//! Button component with multiple variants and sizes
//!
//! Mirrors the shadcn/ui Button component API.

use gpui::{
    div, px, App, ClickEvent, Div, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window,
};

use crate::theme::Theme;

/// Button style variants
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ButtonVariant {
    #[default]
    Default,
    Secondary,
    Outline,
    Destructive,
    Ghost,
    Link,
}

/// Button size variants
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ButtonSize {
    Xs,
    Sm,
    #[default]
    Default,
    Lg,
    Icon,
    IconXs,
    IconSm,
    IconLg,
}

/// A button component with various style and size variants
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    size: ButtonSize,
    disabled: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl Button {
    /// Create a new button with the given label
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::default(),
            size: ButtonSize::default(),
            disabled: false,
            on_click: None,
        }
    }

    /// Create a new icon-only button
    pub fn icon(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            label: SharedString::default(),
            variant: ButtonVariant::default(),
            size: ButtonSize::Icon,
            disabled: false,
            on_click: None,
        }
    }

    /// Set the button variant
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Set the button size
    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    /// Set the button as disabled
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set the click handler
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    fn get_colors(&self, theme: &Theme) -> (Hsla, Hsla, Hsla) {
        match self.variant {
            ButtonVariant::Default => (theme.primary, theme.primary_foreground, theme.primary),
            ButtonVariant::Secondary => {
                (theme.secondary, theme.secondary_foreground, theme.secondary)
            }
            ButtonVariant::Outline => (theme.background, theme.foreground, theme.border),
            ButtonVariant::Destructive => {
                (theme.destructive, theme.destructive_foreground, theme.destructive)
            }
            ButtonVariant::Ghost => (
                gpui::hsla(0.0, 0.0, 0.0, 0.0),
                theme.foreground,
                gpui::hsla(0.0, 0.0, 0.0, 0.0),
            ),
            ButtonVariant::Link => (
                gpui::hsla(0.0, 0.0, 0.0, 0.0),
                theme.primary,
                gpui::hsla(0.0, 0.0, 0.0, 0.0),
            ),
        }
    }

    fn get_hover_bg(&self, theme: &Theme) -> Hsla {
        match self.variant {
            ButtonVariant::Default => gpui::hsla(
                theme.primary.h,
                theme.primary.s,
                theme.primary.l * 0.9,
                theme.primary.a,
            ),
            ButtonVariant::Secondary => gpui::hsla(
                theme.secondary.h,
                theme.secondary.s,
                theme.secondary.l * 0.8,
                theme.secondary.a,
            ),
            ButtonVariant::Outline | ButtonVariant::Ghost => theme.accent,
            ButtonVariant::Destructive => gpui::hsla(
                theme.destructive.h,
                theme.destructive.s,
                theme.destructive.l * 0.9,
                theme.destructive.a,
            ),
            ButtonVariant::Link => gpui::hsla(0.0, 0.0, 0.0, 0.0),
        }
    }

    fn get_size_styles(&self) -> (f32, f32, f32, f32) {
        // Returns (height, padding_x, padding_y, font_size)
        match self.size {
            ButtonSize::Xs => (24.0, 8.0, 2.0, 12.0),
            ButtonSize::Sm => (32.0, 12.0, 4.0, 13.0),
            ButtonSize::Default => (36.0, 16.0, 8.0, 14.0),
            ButtonSize::Lg => (40.0, 24.0, 10.0, 14.0),
            ButtonSize::Icon => (36.0, 0.0, 0.0, 14.0),
            ButtonSize::IconXs => (24.0, 0.0, 0.0, 12.0),
            ButtonSize::IconSm => (32.0, 0.0, 0.0, 13.0),
            ButtonSize::IconLg => (40.0, 0.0, 0.0, 14.0),
        }
    }

    /// Render the button into a GPUI element
    pub fn render(self) -> Stateful<Div> {
        let theme = Theme::default();
        let (bg_color, text_color, border_color) = self.get_colors(&theme);
        let hover_bg = self.get_hover_bg(&theme);
        let (height, px_padding, _py_padding, font_size) = self.get_size_styles();

        let is_icon = matches!(
            self.size,
            ButtonSize::Icon | ButtonSize::IconXs | ButtonSize::IconSm | ButtonSize::IconLg
        );

        let mut base = div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(height))
            .text_size(px(font_size))
            .font_weight(gpui::FontWeight::MEDIUM)
            .rounded(theme.radius_md)
            .bg(bg_color)
            .text_color(text_color)
            .cursor_pointer();

        // Apply border for outline variant
        if matches!(self.variant, ButtonVariant::Outline) {
            base = base.border_1().border_color(border_color);
        }

        // Apply underline for link variant
        if matches!(self.variant, ButtonVariant::Link) {
            base = base.underline();
        }

        // Apply size-specific padding
        if is_icon {
            base = base.w(px(height));
        } else {
            base = base.px(px(px_padding));
        }

        // Apply hover styles
        base = base.hover(|style| style.bg(hover_bg));

        // Apply disabled styles
        if self.disabled {
            base = base.opacity(0.5).cursor_not_allowed();
        } else if let Some(on_click) = self.on_click {
            base = base.on_click(on_click);
        }

        // Add label if not icon-only
        if !self.label.is_empty() {
            base = base.child(self.label.clone());
        }

        base
    }
}

impl IntoElement for Button {
    type Element = Stateful<Div>;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}
