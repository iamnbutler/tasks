//! Markdown for the surfaces that carry it: the orchestrator chat bubble and
//! the inspector's spec and issue body.
//!
//! Shaped as the extension `gpuikit::markdown` wants to grow into rather than
//! a fork of it — same `MarkdownStyle`/`TextStyle` vocabulary, same block
//! boundaries — so when upstream's inline layer catches up, adopting it is a
//! call-site swap. What it fixes today is the inline layer: 0.6.0 flushes an
//! inline link (and an image) as its own top-level block, splitting the
//! sentence around it into three stacked paragraphs, and renders inline code
//! with its backticks still in the text.
//!
//! One engine, two style profiles ([`chat_style`], [`doc_style`]). The
//! surfaces differ in type scale and in which surface colour a code chip sits
//! on — never in how a document is parsed.
//!
//! ```ignore
//! div().child(markdown(chat_key(message.seq), &content, chat_style(cx), cx))
//! ```

pub(crate) mod blocks;
mod render;

use std::collections::HashMap;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, FontWeight, Global, Hsla, SharedString, Window};
use gpuikit::markdown::{MarkdownStyle, TextStyle};
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::models::{SpecId, TaskId};

use crate::workspace::FONT;
use blocks::Block;

/// How many parsed documents to keep. The chat pane holds a window of turns
/// and the inspector one task at a time, so this is generous.
const CACHE_CAPACITY: usize = 128;

/// Parsed documents, keyed by a caller-supplied namespaced key.
///
/// Parsing happens once per content change; the element tree is rebuilt per
/// frame (as Zed does). Keys identify *content*, not parse state, which is why
/// this cache survives a future streaming implementation unchanged.
#[derive(Default)]
pub struct MarkdownStore {
    entries: HashMap<SharedString, Entry>,
    /// Keys least-recently-used first.
    order: Vec<SharedString>,
}

struct Entry {
    source: SharedString,
    blocks: Rc<[Block]>,
}

impl Global for MarkdownStore {}

impl MarkdownStore {
    fn blocks(&mut self, key: &SharedString, source: &str) -> Rc<[Block]> {
        self.touch(key);

        if let Some(entry) = self.entries.get(key) {
            if entry.source == source {
                return entry.blocks.clone();
            }
        }

        let parsed: Rc<[Block]> = blocks::parse(source).into();
        self.entries.insert(
            key.clone(),
            Entry {
                source: source.to_string().into(),
                blocks: parsed.clone(),
            },
        );
        self.evict();
        parsed
    }

    fn touch(&mut self, key: &SharedString) {
        if let Some(ix) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(ix);
        }
        self.order.push(key.clone());
    }

    fn evict(&mut self) {
        while self.order.len() > CACHE_CAPACITY {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }
}

/// A parsed markdown document, ready to render.
#[derive(IntoElement)]
pub struct MarkdownElement {
    key: SharedString,
    blocks: Rc<[Block]>,
    style: MarkdownStyle,
    full_width: bool,
}

impl MarkdownElement {
    /// Stretch to the container's width. Off by default: the chat bubble is
    /// auto-width under `max_w(720)`, and a full-width child would stretch it
    /// to the whole pane.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }
}

impl RenderOnce for MarkdownElement {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        render::document(&self.blocks, &self.style, &self.key, self.full_width, cx)
    }
}

/// Render `source` as markdown, parsing it only when the content behind `key`
/// has changed.
///
/// `key` must be unique per rendered instance within a frame: it namespaces
/// the `InteractiveText` element ids that make links clickable. Use the
/// constructors below rather than minting keys at call sites.
pub fn markdown(
    key: impl Into<SharedString>,
    source: &str,
    style: MarkdownStyle,
    cx: &mut App,
) -> MarkdownElement {
    let key = key.into();
    let blocks = cx.default_global::<MarkdownStore>().blocks(&key, source);
    MarkdownElement {
        key,
        blocks,
        style,
        full_width: false,
    }
}

/// One orchestrator turn. `seq` is server-assigned and append-only, so it
/// identifies content.
pub fn chat_key(seq: i64) -> SharedString {
    format!("chat:{seq}").into()
}

/// A spec's content in the inspector.
pub fn spec_key(id: &SpecId) -> SharedString {
    format!("spec:{id}").into()
}

/// A task's GitHub issue body in the inspector.
pub fn task_body_key(id: &TaskId) -> SharedString {
    format!("task-body:{id}").into()
}

/// Chat bubbles: body at the pane's `text_sm`.
///
/// Code chips sit on `theme.bg()` because the bubble behind them is
/// `surface_secondary` (or the accent wash) — the opposite of [`doc_style`],
/// deliberately.
pub fn chat_style(cx: &App) -> MarkdownStyle {
    profile(0.875, cx.theme().bg(), cx)
}

/// The inspector: body at the pane's `text_xs`, code chips on
/// `surface_secondary` because the pane itself is already `bg`.
pub fn doc_style(cx: &App) -> MarkdownStyle {
    profile(0.75, cx.theme().surface_secondary(), cx)
}

/// The shared profile. Both surfaces differ only in `base` and `code_bg`.
///
/// The type scale is gentler than gpuikit's default (a 1.2 typescale to the
/// fourth power puts `h1` at 29px, which in a chat bubble reads as a shout):
/// headings top out at 1.5× body and the smallest three are body-size bold.
fn profile(base: f32, code_bg: Hsla, cx: &App) -> MarkdownStyle {
    let theme = cx.theme();
    let body = TextStyle {
        size: base,
        line_height: 1.45,
        weight: FontWeight::NORMAL,
        color: Some(theme.fg()),
        margin_top: 0.0,
    };
    let heading = |scale: f32| TextStyle {
        size: base * scale,
        line_height: 1.25,
        weight: FontWeight::BOLD,
        color: Some(theme.fg()),
        margin_top: 0.0,
    };

    MarkdownStyle {
        h1: heading(1.5),
        h2: heading(1.3),
        h3: heading(1.15),
        h4: heading(1.0),
        h5: heading(1.0),
        h6: heading(1.0),
        code: TextStyle {
            size: base * 0.95,
            ..body.clone()
        },
        body,
        code_font_family: FONT.into(),
        block_spacing: 0.4,
        code_block_bg: Some(code_bg),
        code_block_border: Some(theme.border_subtle()),
        inline_code_bg: Some(code_bg),
        block_quote_border: Some(theme.border_secondary()),
        block_quote_text: Some(theme.fg_muted()),
        rule_color: Some(theme.border_subtle()),
        link_color: Some(theme.accent()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_so_two_surfaces_cannot_collide() {
        assert_eq!(chat_key(12).as_ref(), "chat:12");
        let spec = SpecId::from_raw("spec_abc");
        let task = TaskId::from_raw("task_abc");
        assert_eq!(spec_key(&spec).as_ref(), "spec:spec_abc");
        assert_eq!(task_body_key(&task).as_ref(), "task-body:task_abc");
        assert_ne!(spec_key(&spec), task_body_key(&task));
    }

    #[test]
    fn the_cache_reparses_only_when_content_changes() {
        let mut store = MarkdownStore::default();
        let key = chat_key(1);

        let first = store.blocks(&key, "# one");
        let again = store.blocks(&key, "# one");
        assert!(
            Rc::ptr_eq(&first, &again),
            "same content must reuse the parse"
        );

        let changed = store.blocks(&key, "# two");
        assert!(!Rc::ptr_eq(&first, &changed));
        assert_eq!(store.entries.len(), 1, "a key holds one parse, not a pile");
    }

    #[test]
    fn the_cache_evicts_least_recently_used_keys() {
        let mut store = MarkdownStore::default();
        for seq in 0..CACHE_CAPACITY as i64 {
            store.blocks(&chat_key(seq), "body");
        }
        // Re-touch the oldest so it is no longer the eviction candidate.
        store.blocks(&chat_key(0), "body");
        store.blocks(&chat_key(CACHE_CAPACITY as i64), "body");

        assert_eq!(store.entries.len(), CACHE_CAPACITY);
        assert!(store.entries.contains_key(&chat_key(0)));
        assert!(!store.entries.contains_key(&chat_key(1)));
    }
}
