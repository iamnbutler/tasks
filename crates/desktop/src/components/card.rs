//! Card component with header, content, and footer sections
//!
//! Mirrors the shadcn/ui Card component for grouping related content.

use gpui::{div, px, AnyElement, Div, IntoElement, ParentElement, SharedString, Styled};

use crate::theme::Theme;

/// A card container component
pub struct Card {
    children: Vec<AnyElement>,
}

impl Card {
    /// Create a new empty card
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child element to the card
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Add multiple children to the card
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    /// Render the card into a GPUI element
    pub fn render(self) -> Div {
        let theme = Theme::default();

        div()
            .flex()
            .flex_col()
            .gap_6()
            .rounded(theme.radius_xl)
            .border_1()
            .border_color(theme.border)
            .bg(theme.card)
            .text_color(theme.card_foreground)
            .py_6()
            .shadow_sm()
            .children(self.children)
    }
}

impl Default for Card {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for Card {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}

/// Card header section
pub struct CardHeader {
    children: Vec<AnyElement>,
}

impl CardHeader {
    /// Create a new card header
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child element to the header
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Add multiple children to the header
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    /// Render the header into a GPUI element
    pub fn render(self) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_6()
            .children(self.children)
    }
}

impl Default for CardHeader {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for CardHeader {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}

/// Card title component
pub struct CardTitle {
    text: SharedString,
}

impl CardTitle {
    /// Create a new card title
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }

    /// Render the title into a GPUI element
    pub fn render(self) -> Div {
        let theme = Theme::default();

        div()
            .text_color(theme.foreground)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .line_height(px(24.0))
            .child(self.text.clone())
    }
}

impl IntoElement for CardTitle {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}

/// Card description component
pub struct CardDescription {
    text: SharedString,
}

impl CardDescription {
    /// Create a new card description
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }

    /// Render the description into a GPUI element
    pub fn render(self) -> Div {
        let theme = Theme::default();

        div()
            .text_size(px(14.0))
            .text_color(theme.muted_foreground)
            .child(self.text.clone())
    }
}

impl IntoElement for CardDescription {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}

/// Card content section
pub struct CardContent {
    children: Vec<AnyElement>,
}

impl CardContent {
    /// Create a new card content section
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child element to the content
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Add multiple children to the content
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    /// Render the content into a GPUI element
    pub fn render(self) -> Div {
        div().px_6().children(self.children)
    }
}

impl Default for CardContent {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for CardContent {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}

/// Card footer section
pub struct CardFooter {
    children: Vec<AnyElement>,
}

impl CardFooter {
    /// Create a new card footer
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Add a child element to the footer
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    /// Add multiple children to the footer
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(|c| c.into_any_element()));
        self
    }

    /// Render the footer into a GPUI element
    pub fn render(self) -> Div {
        div().flex().items_center().px_6().children(self.children)
    }
}

impl Default for CardFooter {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoElement for CardFooter {
    type Element = Div;

    fn into_element(self) -> Self::Element {
        self.render()
    }
}
