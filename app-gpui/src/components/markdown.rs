//! Markdown rendering shared by every reading surface — chat replies now,
//! specs, task bodies, and eventually transcript views.
//!
//! Two pieces. [`MarkdownCache`] keeps one parsed `Markdown` entity per
//! stable key so a re-render doesn't re-parse every message on every frame
//! (the chat re-renders once a second while work is live). [`markdown_block`]
//! styles the element for this app: chat-scale type, Menlo code, theme
//! colors — headings deliberately modest, because these render inside a
//! conversation, not a document page.
//!
//! The cache also decides *how* a key's text changed, which is what makes the
//! chat's streaming rows behave: see [`Update`].

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use gpui::prelude::*;
use gpui::{div, App, ElementId, Entity, EntityId, FontWeight, SharedString};
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
    ///
    /// Callers pass the whole text they want rendered and say nothing about
    /// how it got there — the *shape* of the text decides (see [`Update`]), so
    /// there is no second entry point for a caller to pick wrong. The chat's
    /// live row grows by pure suffix and streams; every other surface
    /// (`spec:{id}`, `task:{id}`, the durable `chat:{seq}`) changes
    /// in ways that are not extensions and keeps the replacing path.
    pub fn entity(
        &mut self,
        key: impl Into<SharedString>,
        source: &str,
        cx: &mut App,
    ) -> Entity<Markdown> {
        match self.entries.entry(key.into()) {
            Entry::Occupied(entry) => {
                let entity = entry.get().clone();
                match Update::between(entity.read(cx).source(), source) {
                    Update::Unchanged => {}
                    Update::Append(tail) => {
                        entity.update(cx, |markdown, cx| markdown.append(&tail, cx));
                    }
                    Update::Replace(source) => {
                        entity.update(cx, |markdown, cx| markdown.set_source(source, cx));
                    }
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
    /// serves chat, specs and task bodies, so eviction is per-key on purpose:
    /// a chat reset must not throw away spec parses. Without it the cache
    /// grows by one orphaned parse per turn, forever.
    pub fn remove(&mut self, key: impl Into<SharedString>) {
        self.entries.remove(&key.into());
    }
}

/// How the text behind a cached key changed since the last frame.
///
/// The chat's live row (`chat:entry:{id}`) is fed by `ChatLog::push_delta`,
/// which is a pure `push_str` — so a streaming reply reaches us as a strictly
/// growing string, and that is exactly what makes this classification exact
/// rather than heuristic.
///
/// Appending is not a parsing optimization: `Markdown::append` rebuilds the
/// whole source and re-parses all of it, same as `set_source`. What it buys is
/// the **selection** — `set_source` drops it, because its offsets belong to
/// the old text, so today text selected inside a reply is destroyed by the
/// next delta. Appending keeps it, since text arriving at the end of a
/// document cannot disturb a selection made earlier in it. (The cost of
/// parsing on the render path is gone too, but that comes from the gpuikit
/// upgrade — it parses in the background and coalesces deltas — and both
/// paths get it.)
///
/// **Classify against `Markdown::source`, never `parsed_source`.** That is the
/// one way to get this wrong that corrupts the chat rather than merely slowing
/// it. gpuikit parses in the background, so `parsed_source` lags `source`
/// while a parse is in flight, while `append` concatenates onto `source`.
/// Classifying against the lagging one returns an [`Update::Append`] of text
/// that is already in the document, duplicating every delta that arrives
/// during a parse — and with a background parser that is the common case, not
/// the rare one.
#[derive(Debug, PartialEq, Eq)]
enum Update {
    /// The text is what the entity already holds.
    ///
    /// A distinct arm rather than an empty [`Update::Append`], because while
    /// `Markdown::append` does early-return on empty text, *reaching* it still
    /// costs an `entity.update` and the notify inside it — every frame, for
    /// every row on screen. Finished messages sit here forever, so this is the
    /// steady state and not an edge case.
    Unchanged,
    /// The new text extends the old; the payload is only the new tail.
    Append(String),
    /// Anything else — an edit, a truncation, a different document.
    Replace(String),
}

impl Update {
    /// Classify `next` against the text an entity currently holds.
    fn between(current: &str, next: &str) -> Self {
        match next.strip_prefix(current) {
            // Identical text strips to an empty remainder, so this arm is
            // also the equality check and has to come first.
            Some("") => Update::Unchanged,
            Some(tail) => Update::Append(tail.to_string()),
            None => Update::Replace(next.to_string()),
        }
    }
}

/// The id of the wrapper every one of a document's elements hangs under.
///
/// gpui hashes an element's *whole* id path into an accessibility node id and
/// refuses duplicates, so this is what makes gpuikit's per-render text-run ids
/// unique across a frame that draws more than one document. Keyed on the
/// entity because [`MarkdownCache`] hands out one stable entity per key, so
/// the id is unique per document *and* stable across frames — assistive
/// technology reads a changed node id as a different element.
///
/// Deliberately the same shape as gpuikit's own `element_id::for_entity`,
/// which is where upstream landed in gpuikit #145, and exactly what
/// `.id(("markdown", entity_id))` already produced: gpui's
/// `From<(&'static str, EntityId)> for ElementId` (`window.rs:6533`) is this
/// same expression. Naming it is what makes it testable.
fn block_element_id(entity_id: EntityId) -> ElementId {
    ElementId::NamedInteger("markdown".into(), entity_id.as_u64())
}

/// A markdown element styled for this app's reading surfaces.
///
/// The wrapper `div` exists for its id, not its box, and the id is not
/// redundant with anything upstream does. gpuikit mints text-run ids
/// (`md-run-1`, `md-run-2`, …) from a counter it restarts on every render, so
/// run ids are unique only *within* one document; [`block_element_id`] is what
/// makes them unique across a frame, at any gpuikit version — including the
/// next one.
///
/// The history is the argument for keeping it. Two markdown documents on one
/// screen collided in the a11y tree's uniqueness assert (#861, back when Home
/// rendered three briefings at once) and this wrapper is the fix that shipped
/// (#882). gpuikit 0.7.0 still minted colliding ids afterwards and was merely
/// *inert*, because its text runs reported no a11y role and gpui builds a node
/// only for an element that has one. gpuikit #133 — an ancestor of the rev
/// this app pins — scoped the run ids under a per-document element and gave
/// those runs their roles back, which is precisely the re-arming #882
/// predicted, and was safe only because the same commit did both. Deleting
/// this wrapper on the grounds that upstream now scopes its own ids bets the
/// crash on that ordering holding forever.
pub fn markdown_block(entity: &Entity<Markdown>, cx: &App) -> impl IntoElement {
    div()
        .id(block_element_id(entity.entity_id()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_unchanged() {
        assert_eq!(Update::between("hello", "hello"), Update::Unchanged);
        assert_eq!(Update::between("", ""), Update::Unchanged);
    }

    #[test]
    fn a_streaming_delta_appends_only_the_new_tail() {
        assert_eq!(
            Update::between("The **spec", "The **spec** says"),
            Update::Append("** says".into())
        );
    }

    #[test]
    fn the_first_delta_of_a_document_appends_all_of_it() {
        assert_eq!(
            Update::between("", "Working on it"),
            Update::Append("Working on it".into())
        );
    }

    #[test]
    fn a_tail_of_pure_whitespace_still_appends() {
        // Non-empty is the whole test: `Unchanged` is equality and nothing
        // else, so a delta that is only a newline must reach the document.
        assert_eq!(Update::between("one", "one\n"), Update::Append("\n".into()));
    }

    #[test]
    fn text_that_shrank_is_replaced() {
        assert_eq!(
            Update::between("a longer draft", "a longer"),
            Update::Replace("a longer".into())
        );
        assert_eq!(Update::between("gone", ""), Update::Replace("".into()));
    }

    #[test]
    fn text_that_diverged_is_replaced() {
        // A shared prefix is not enough — a spec re-fetched with an edit in
        // the middle of it must not be classified as a stream.
        assert_eq!(
            Update::between("## Summary\nold", "## Summary\nnew"),
            Update::Replace("## Summary\nnew".into())
        );
        assert_eq!(
            Update::between("task:41", "briefing"),
            Update::Replace("briefing".into())
        );
    }

    /// The property the whole a11y defence rests on: two documents drawn in
    /// one frame hang under two different ids, so the run ids underneath them
    /// cannot collide however upstream numbers them.
    #[test]
    fn two_documents_get_two_ids() {
        assert_ne!(
            block_element_id(EntityId::from(1u64)),
            block_element_id(EntityId::from(2u64))
        );
    }

    /// The other half: stable frame to frame. A node whose id changed is a
    /// different element as far as assistive technology is concerned, so a
    /// per-frame counter here would be its own bug.
    #[test]
    fn one_document_keeps_its_id_across_frames() {
        let entity = EntityId::from(7u64);
        assert_eq!(block_element_id(entity), block_element_id(entity));
    }

    /// A name *qualified by the entity*, not a bare name — a bare `"markdown"`
    /// would collide for exactly the documents this exists to separate.
    ///
    /// Asserted against `as_u64()` rather than the literal `9`: `EntityId` is
    /// a slotmap key and packs a version into the high half, so
    /// `EntityId::from(9u64).as_u64()` is not 9. Pinning the literal would pin
    /// slotmap's packing, which is not the property under test.
    #[test]
    fn the_id_is_a_name_qualified_by_the_entity() {
        let entity = EntityId::from(9u64);
        assert_eq!(
            block_element_id(entity),
            ElementId::NamedInteger("markdown".into(), entity.as_u64())
        );
    }

    /// The `stitch` feature closes syntax a half-streamed document leaves open
    /// (`**bold` flashing as literal asterisks one delta before it becomes
    /// bold). It is named in exactly one place — the gpuikit line in
    /// `Cargo.toml` — and nothing else in the app refers to it, so a routine
    /// dependency edit would drop it silently and the only symptom would be
    /// visual, on a platform this crate cannot run on.
    #[test]
    fn partial_markdown_preprocessing_is_compiled_in() {
        assert!(
            gpuikit::markdown::preprocessing_available(),
            "the `stitch` feature is not enabled on the gpuikit dependency"
        );
    }
}
