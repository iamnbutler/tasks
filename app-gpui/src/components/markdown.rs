//! Markdown rendering shared by every reading surface — chat replies now,
//! specs, briefings, and eventually transcript views.
//!
//! Two pieces. [`MarkdownCache`] keeps one parsed `Markdown` entity per
//! stable key so a re-render doesn't re-parse every message on every frame
//! (the chat re-renders once a second while work is live). [`markdown_block`]
//! styles the element for this app: chat-scale type, Menlo code, theme
//! colors — headings deliberately modest, because these render inside a
//! conversation, not a document page.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use gpui::prelude::*;
use gpui::{div, App, Entity, FontWeight, SharedString};
use gpuikit::markdown::{Markdown, MarkdownElement, MarkdownStyle, TextStyle};
use gpuikit::theme::{ActiveTheme, Themeable};

use crate::workspace::FONT;

/// Parsed-markdown entities keyed by a caller-chosen stable string
/// (`"chat:{seq}"`, `"spec:{id}"`, …). Sources are re-parsed only when the
/// text behind a key actually changes.
#[derive(Default)]
pub struct MarkdownCache {
    entries: HashMap<SharedString, Entity<Markdown>>,
}

impl MarkdownCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The entity for `key`, created or updated to hold `source`.
    pub fn entity(
        &mut self,
        key: impl Into<SharedString>,
        source: &str,
        cx: &mut App,
    ) -> Entity<Markdown> {
        match self.entries.entry(key.into()) {
            Entry::Occupied(entry) => {
                let entity = entry.get().clone();
                if entity.read(cx).source() != source {
                    let source = source.to_string();
                    entity.update(cx, |markdown, cx| markdown.set_source(source, cx));
                }
                entity
            }
            Entry::Vacant(slot) => {
                let source = source.to_string();
                let entity = cx.new(|cx| Markdown::new(source, cx));
                slot.insert(entity.clone());
                entity
            }
        }
    }

    /// Drop one cached entry — for a key that has left its surface (a chat
    /// row that retired, a seq that will never be asked for again). One cache
    /// serves chat, specs and briefings, so eviction is per-key on purpose:
    /// a chat reset must not throw away spec parses. Without it the cache
    /// grows by one orphaned parse per turn, forever.
    pub fn remove(&mut self, key: impl Into<SharedString>) {
        self.entries.remove(&key.into());
    }
}

/// A markdown element styled for this app's reading surfaces.
///
/// The wrapper `div` exists for its id, not its box: gpuikit mints text-run
/// ids (`md-run-1`, `md-run-2`, …) from a counter it restarts on every
/// render, so two documents on one screen emit the same ids and collide in
/// whatever gpui keys off the id path — which is how three briefings on Home
/// tripped the a11y tree's uniqueness assert (#861). Giving each document a
/// distinct id ancestor makes the app immune regardless of what upstream does
/// with that counter next. Keyed on `entity_id()` because [`MarkdownCache`]
/// already hands out one stable entity per key, so the id is unique per
/// document *and* stable across frames — which is what gpui wants in order to
/// treat a node as the same node frame to frame.
pub fn markdown_block(entity: &Entity<Markdown>, cx: &App) -> impl IntoElement {
    div()
        .id(("markdown", entity.entity_id()))
        .w_full()
        .child(MarkdownElement::new(entity.clone()).style(style(cx)))
}

fn style(cx: &App) -> MarkdownStyle {
    let theme = cx.theme();

    // The app's body text is `text_sm` (0.875rem); markdown must sit flush
    // with it rather than importing a document-scale hierarchy.
    let base = 0.875;
    let heading = |size: f32| TextStyle {
        size,
        line_height: 1.3,
        weight: FontWeight::BOLD,
        color: None,
        margin_top: 0.5,
    };

    let mut style = MarkdownStyle::new()
        .code_font(FONT)
        // Every surface this style serves renders agent- or GitHub-authored
        // markdown, where a single newline is an intended break.
        .soft_break_as_hard_break(true)
        .link_color(theme.accent())
        .code_colors(theme.surface_secondary(), theme.border_subtle())
        .block_quote_colors(theme.border_secondary(), theme.fg_muted())
        .rule_color(theme.border_subtle());
    style.body = TextStyle {
        size: base,
        line_height: 1.5,
        ..TextStyle::body()
    };
    style.code = TextStyle {
        size: base * 0.93,
        line_height: 1.5,
        ..TextStyle::code()
    };
    style.h1 = heading(base * 1.3);
    style.h2 = heading(base * 1.15);
    style.h3 = heading(base * 1.05);
    style.h4 = heading(base);
    style.h5 = heading(base);
    style.h6 = heading(base);
    style
}
