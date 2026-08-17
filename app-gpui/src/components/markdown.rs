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

/// Turn on syntax highlighting for fenced code blocks, process-wide.
///
/// A thin forward to gpuikit, so the app's one dependency on the `editor`
/// feature sits with the rest of its markdown wiring rather than in `main`.
/// Call it once at startup; it only writes a global, and every read of that
/// global is behind a `try_global`, so it is order-independent and every step
/// below it degrades to plain monospace rather than failing: no feature, no
/// init, no info string on the fence, an info string syntect has no grammar
/// for, or a block over 256 KiB.
///
/// No theme call is needed. `CodeHighlightTheme::FollowApp` is the default and
/// picks a dark or light syntect theme from the lightness of the surface each
/// block is painted on, so a theme change is followed with nothing observing
/// it.
///
/// **Highlighting a *streaming* fence is expensive**, and it was measured
/// rather than guessed. gpuikit's highlight cache keys on the whole block
/// text, so a fence arriving through `Markdown::append` misses on every delta
/// and pays a full syntect pass over the block-so-far — and `state.rs`
/// notifies on every delta, so that is per rendered frame. Release build,
/// aarch64: a 20-line fence costs 3.4 ms settled and 25 ms streamed; 100 lines
/// 8.4 ms and 457 ms; 400 lines 15.2 ms settled and 5.32 s streamed, whose
/// worst delta is ~16.3 ms — the whole 60 fps budget. One long streamed fence
/// also deposits hundreds of prefix entries and clears the 256-entry cache
/// wholesale, evicting the settled spec and task blocks with it.
///
/// It ships anyway, and there is no app-side lever to ship instead:
/// highlighting is a process-global on/off and the per-block decision lives
/// inside gpuikit's `code_block`. The reading surfaces are static and win
/// outright; chat degrades only while a fence is actively streaming. The cheap
/// upstream fix is to skip highlighting a fence `stitch` had to close —
/// `close_open_syntax` returns `Cow::Owned` exactly when the source was
/// incomplete, which is precisely the streaming case.
pub fn init_code_highlighting(cx: &mut App) {
    gpuikit::markdown::init_code_highlighting(cx);
}

/// Parsed-markdown entities keyed by a caller-chosen stable string
/// (`"chat:{seq}"`, `"spec:{id}"`, …). Sources are re-parsed only when the
/// text behind a key actually changes.
#[derive(Default)]
pub struct MarkdownCache {
    entries: HashMap<SharedString, Entity<Markdown>>,
    /// The key of the document whose selection ⌘C copies, if any.
    ///
    /// gpuikit's selection is per-document and its clear-on-press-outside
    /// handler is registered *during paint*, so a document that stopped being
    /// drawn is never told to let go: select a task body, switch section,
    /// select a briefing line, and two documents hold a selection at once with
    /// nothing upstream able to arbitrate — nothing upstream knows the two
    /// share a window. This field is the arbitration, maintained by
    /// [`MarkdownCache::sync_selection`].
    active_selection: Option<SharedString>,
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
        let key = key.into();
        self.entries.remove(&key);
        // A retired key can never be resolved back to an entity, so leaving it
        // named here would make `selected_text` answer `None` forever rather
        // than falling through to whatever the user selects next.
        if self.active_selection.as_ref() == Some(&key) {
            self.active_selection = None;
        }
    }

    /// Decide which document's selection is live this frame, and drop every
    /// other one. Call once per frame, before anything renders.
    ///
    /// Idempotent by construction: clearing a selection is persistent, so a
    /// displaced document is absent from `selected` on the next pass and
    /// nothing notifies again. That matters — the `cx.notify()` below is what
    /// repaints away the stale highlight, and a clear that kept re-firing
    /// would schedule a render on every frame forever.
    pub fn sync_selection(&mut self, cx: &mut App) {
        let selected: Vec<SharedString> = self
            .entries
            .iter()
            .filter(|(_, entity)| !entity.read(cx).selection().is_empty())
            .map(|(key, _)| key.clone())
            .collect();

        let active = resolve_active(&selected, self.active_selection.as_ref());

        for key in &selected {
            if Some(key) == active.as_ref() {
                continue;
            }
            let Some(entity) = self.entries.get(key).cloned() else {
                continue;
            };
            entity.update(cx, |markdown, cx| {
                markdown.selection().clear();
                cx.notify();
            });
        }

        self.active_selection = active;
    }

    /// What ⌘C should copy: the text selected in the active document.
    ///
    /// Reads only the active key rather than scanning, which is the whole
    /// point of [`MarkdownCache::sync_selection`] — a scan over a `HashMap`
    /// would copy whichever stale selection iteration happened to yield first.
    ///
    /// `None` for a selected document that is off screen, because upstream
    /// reconstructs the text from a registry it rebuilds each paint. That is
    /// the honest answer rather than something to route around: the user
    /// cannot see the highlight either.
    pub fn selected_text(&self, cx: &App) -> Option<String> {
        let key = self.active_selection.as_ref()?;
        self.entries.get(key)?.read(cx).selected_text()
    }
}

/// Which of the documents holding a selection this frame owns it.
///
/// A key that is selected now and was not active is the drag the user just
/// made — there is one pointer, so at most one newcomer per frame — and it
/// wins. Otherwise the incumbent keeps it, and everything else in `selected`
/// is a stale selection in a document that stopped being painted.
fn resolve_active(
    selected: &[SharedString],
    active: Option<&SharedString>,
) -> Option<SharedString> {
    selected
        .iter()
        .find(|key| Some(*key) != active)
        .or_else(|| selected.first())
        .cloned()
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

    fn key(name: &str) -> SharedString {
        SharedString::from(name.to_string())
    }

    #[test]
    fn nothing_selected_owns_nothing() {
        assert_eq!(resolve_active(&[], None), None);
        assert_eq!(resolve_active(&[], Some(&key("task:1"))), None);
    }

    #[test]
    fn the_only_selection_is_the_active_one() {
        assert_eq!(resolve_active(&[key("task:1")], None), Some(key("task:1")));
    }

    /// The drag the user just made wins. One pointer means at most one key can
    /// have gone from unselected to selected since the last frame, so "the one
    /// that is not the incumbent" identifies it exactly.
    #[test]
    fn a_newly_selected_document_takes_it_from_the_incumbent() {
        assert_eq!(
            resolve_active(&[key("task:1"), key("brief:queue")], Some(&key("task:1"))),
            Some(key("brief:queue"))
        );
    }

    /// The settled frame, and the reason `sync_selection` is idempotent: with
    /// only the incumbent selected there is no newcomer, so it keeps the
    /// selection and nothing is displaced — hence nothing to clear and nothing
    /// to notify.
    #[test]
    fn a_settled_frame_leaves_the_active_document_alone() {
        let active = key("task:1");
        assert_eq!(
            resolve_active(std::slice::from_ref(&active), Some(&active)),
            Some(active)
        );
    }

    /// The incumbent's own document cleared its selection (a click landed
    /// outside it) while another still holds one: the survivor takes over
    /// rather than leaving ⌘C pointed at an empty document.
    #[test]
    fn a_survivor_takes_over_from_an_incumbent_that_let_go() {
        assert_eq!(
            resolve_active(&[key("spec:7")], Some(&key("task:1"))),
            Some(key("spec:7"))
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
