//! Blocks → gpui elements.
//!
//! Each block becomes exactly one `StyledText` carrying highlight ranges, so
//! emphasis, inline code and links stay *inside* the sentence instead of
//! becoming stacked blocks. A block that carries links is wrapped in
//! `InteractiveText` with click ranges that open the URL.
//!
//! The element tree is rebuilt every frame (as Zed does); only parsing is
//! cached, in [`super::MarkdownStore`].

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, px, rems, AnyElement, App, FontStyle, FontWeight, Hsla, InteractiveText, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle,
};
use gpuikit::markdown::{MarkdownStyle, TextStyle};
use gpuikit::theme::{ActiveTheme, Themeable};

use super::blocks::{Align, Block, Cell, Marker, Span};

/// Render a whole document.
///
/// `key` namespaces the `InteractiveText` element ids: they must be unique
/// per frame, so rendering the same key twice in one frame would collide.
pub(super) fn document(
    blocks: &[Block],
    style: &MarkdownStyle,
    key: &SharedString,
    full_width: bool,
    cx: &App,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(rems(style.block_spacing))
        // Opt-in: the chat bubble is auto-width under `max_w(720)`, and a
        // full-width child would stretch it to the whole pane.
        .when(full_width, |el| el.w_full())
        .children(
            blocks
                .iter()
                .enumerate()
                .map(|(ix, block)| block_element(block, style, format!("{key}-{ix}").into(), cx)),
        )
        .into_any_element()
}

fn block_element(block: &Block, style: &MarkdownStyle, id: SharedString, cx: &App) -> AnyElement {
    match block {
        Block::Heading { level, spans } => {
            let text_style = heading_style(*level, style);
            text_block(spans, style, text_style, id, cx).into_any_element()
        }
        Block::Paragraph { spans } => {
            text_block(spans, style, &style.body, id, cx).into_any_element()
        }
        Block::ListItem {
            marker,
            depth,
            spans,
        } => list_item(*marker, *depth, spans, style, id, cx),
        Block::Code { language, text } => code_block(language.as_deref(), text, style, cx),
        Block::Quote { blocks } => quote(blocks, style, id, cx),
        Block::Table {
            alignments,
            header,
            rows,
        } => table(alignments, header, rows, style, id, cx),
        Block::Rule => div()
            .h(px(1.))
            .w_full()
            .bg(style
                .rule_color
                .unwrap_or_else(|| cx.theme().border_subtle()))
            .into_any_element(),
    }
}

fn heading_style(level: u8, style: &MarkdownStyle) -> &TextStyle {
    match level {
        1 => &style.h1,
        2 => &style.h2,
        3 => &style.h3,
        4 => &style.h4,
        5 => &style.h5,
        _ => &style.h6,
    }
}

/// One text run: a sized, coloured container wrapping the styled text so the
/// inline highlights layer on top of an inherited base style.
fn text_block(
    spans: &[Span],
    style: &MarkdownStyle,
    text_style: &TextStyle,
    id: SharedString,
    cx: &App,
) -> impl IntoElement {
    let color = text_style.color.unwrap_or_else(|| cx.theme().fg());
    div()
        .text_size(rems(text_style.size))
        .line_height(rems(text_style.size * text_style.line_height))
        .font_weight(text_style.weight)
        .text_color(color)
        .child(inline_element(spans, style, id, cx))
}

fn list_item(
    marker: Marker,
    depth: usize,
    spans: &[Span],
    style: &MarkdownStyle,
    id: SharedString,
    cx: &App,
) -> AnyElement {
    let text_style = &style.body;
    let color = text_style.color.unwrap_or_else(|| cx.theme().fg());
    div()
        .flex()
        .flex_row()
        .items_start()
        .pl(rems(depth as f32 * 1.25))
        .text_size(rems(text_style.size))
        .line_height(rems(text_style.size * text_style.line_height))
        .text_color(color)
        .child(
            // Fixed width, always present: a continuation row keeps the
            // column (empty) so its text lines up under the first line.
            div()
                .flex_none()
                .w(rems(text_style.size * 1.6))
                .child(marker_label(marker)),
        )
        .child(div().flex_1().child(inline_element(spans, style, id, cx)))
        .into_any_element()
}

/// Checkbox and bullet glyphs come out of font fallback (the whole app runs in
/// Menlo); if they ever render as tofu, ASCII is the swap.
fn marker_label(marker: Marker) -> SharedString {
    match marker {
        Marker::Bullet => "•".into(),
        Marker::Ordered(n) => format!("{n}.").into(),
        Marker::Task(true) => "☑".into(),
        Marker::Task(false) => "☐".into(),
        Marker::Continuation => "".into(),
    }
}

fn code_block(language: Option<&str>, text: &str, style: &MarkdownStyle, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let code = &style.code;
    div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(6.))
        .border_1()
        .border_color(
            style
                .code_block_border
                .unwrap_or_else(|| theme.border_subtle()),
        )
        .bg(style
            .code_block_bg
            .unwrap_or_else(|| theme.surface_secondary()))
        .overflow_hidden()
        .when_some(language, |el, language| {
            el.child(
                div()
                    .px(px(8.))
                    .pt(px(4.))
                    .text_size(rems(code.size * 0.85))
                    .text_color(theme.fg_muted())
                    .child(language.to_string()),
            )
        })
        // The one seam syntax highlighting would take: swap this child for a
        // `Vec<TextRun>` from gpuikit's `editor` feature.
        .child(
            div()
                .px(px(8.))
                .py(px(6.))
                .font_family(style.code_font_family.clone())
                .text_size(rems(code.size))
                .line_height(rems(code.size * code.line_height))
                .text_color(code.color.unwrap_or_else(|| theme.fg()))
                .child(text.to_string()),
        )
        .into_any_element()
}

fn quote(blocks: &[Block], style: &MarkdownStyle, id: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    // The quoted body is muted; nested quotes inherit the muting because the
    // recursion carries the modified style down.
    let mut inner = style.clone();
    inner.body.color = Some(style.block_quote_text.unwrap_or_else(|| theme.fg_muted()));

    div()
        .flex()
        .flex_col()
        .gap(rems(style.block_spacing))
        .pl(px(8.))
        .border_l(px(2.))
        .border_color(
            style
                .block_quote_border
                .unwrap_or_else(|| theme.border_secondary()),
        )
        .children(
            blocks
                .iter()
                .enumerate()
                .map(|(ix, block)| block_element(block, &inner, format!("{id}-q{ix}").into(), cx)),
        )
        .into_any_element()
}

fn table(
    alignments: &[Align],
    header: &[Cell],
    rows: &[Vec<Cell>],
    style: &MarkdownStyle,
    id: SharedString,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let border = theme.border_subtle();

    let row_element = |cells: &[Cell], row_ix: usize, is_header: bool| {
        div()
            .flex()
            .flex_row()
            .items_start()
            .when(row_ix > 0 || is_header, |el| {
                el.border_t_1().border_color(border)
            })
            .when(is_header, |el| el.bg(theme.surface_secondary()))
            .children(cells.iter().enumerate().map(|(col, cell)| {
                let cell_id: SharedString = if is_header {
                    format!("{id}-th{col}").into()
                } else {
                    format!("{id}-tr{row_ix}c{col}").into()
                };
                div()
                    .flex_1()
                    .px(px(6.))
                    .py(px(3.))
                    .when(col > 0, |el| el.border_l_1().border_color(border))
                    .when(is_header, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .map(
                        |el| match alignments.get(col).copied().unwrap_or_default() {
                            Align::Default | Align::Left => el,
                            Align::Center => el.text_center(),
                            Align::Right => el.text_right(),
                        },
                    )
                    .child(inline_element(cell, style, cell_id, cx))
            }))
    };

    let text_style = &style.body;
    div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .overflow_hidden()
        .text_size(rems(text_style.size))
        .line_height(rems(text_style.size * text_style.line_height))
        .text_color(text_style.color.unwrap_or_else(|| theme.fg()))
        .when(!header.is_empty(), |el| {
            el.child(row_element(header, 0, true))
        })
        .children(
            rows.iter()
                .enumerate()
                .map(|(ix, cells)| row_element(cells, ix, false)),
        )
        .into_any_element()
}

/// The inline layer: one `StyledText` for the whole run, with highlights,
/// font-family overrides for code, and click ranges for links.
fn inline_element(spans: &[Span], style: &MarkdownStyle, id: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let code_bg = style
        .inline_code_bg
        .unwrap_or_else(|| theme.surface_secondary());
    let link_color = style.link_color.unwrap_or_else(|| theme.accent());

    let inline = Inline::build(spans, &style.code_font_family, code_bg, link_color);
    let text = StyledText::new(inline.text).with_highlights(inline.highlights);
    let text = if inline.fonts.is_empty() {
        text
    } else {
        text.with_font_family_overrides(inline.fonts)
    };

    if inline.links.is_empty() {
        return text.into_any_element();
    }

    let (ranges, urls): (Vec<Range<usize>>, Vec<SharedString>) = inline.links.into_iter().unzip();
    InteractiveText::new(id, text)
        .on_click(ranges, move |ix, _window, cx| {
            if let Some(url) = urls.get(ix) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

/// The flattened inline run: text plus the parallel range lists gpui wants.
#[derive(Debug, Default)]
struct Inline {
    text: SharedString,
    highlights: Vec<(Range<usize>, gpui::HighlightStyle)>,
    fonts: Vec<(Range<usize>, SharedString)>,
    links: Vec<(Range<usize>, SharedString)>,
}

impl Inline {
    /// Ranges come out sorted, non-overlapping and on char boundaries *by
    /// construction* — one per span, appended in source order — which is what
    /// `StyledText::with_highlights` debug-asserts. Anything that later adds a
    /// second highlight source (search, selection) has to merge rather than
    /// append, or it panics in debug builds.
    fn build(spans: &[Span], code_font: &SharedString, code_bg: Hsla, link_color: Hsla) -> Self {
        let mut inline = Inline::default();
        let mut text = String::new();

        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            let start = text.len();
            text.push_str(&span.text);
            let range = start..text.len();

            if span.emphasis.is_plain() && span.link.is_none() {
                continue;
            }

            let mut highlight = gpui::HighlightStyle::default();
            if span.emphasis.bold {
                highlight.font_weight = Some(FontWeight::BOLD);
            }
            if span.emphasis.italic {
                highlight.font_style = Some(FontStyle::Italic);
            }
            if span.emphasis.strikethrough {
                highlight.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    ..Default::default()
                });
            }
            if span.emphasis.code {
                // The whole app renders in Menlo, so the font override cannot
                // be what distinguishes code — the chip behind it does that.
                // The override is kept for the day the UI font stops being
                // monospace.
                highlight.background_color = Some(code_bg);
                inline.fonts.push((range.clone(), code_font.clone()));
            }
            if let Some(url) = &span.link {
                highlight.color = Some(link_color);
                highlight.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(link_color),
                    wavy: false,
                });
                inline.links.push((range.clone(), url.clone().into()));
            }
            inline.highlights.push((range, highlight));
        }

        inline.text = text.into();
        inline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::blocks::{parse, Emphasis};

    fn span(text: &str, emphasis: Emphasis, link: Option<&str>) -> Span {
        Span {
            text: text.to_string(),
            emphasis,
            link: link.map(str::to_string),
        }
    }

    fn build(spans: &[Span]) -> Inline {
        Inline::build(
            spans,
            &SharedString::from("Menlo"),
            gpui::hsla(0., 0., 0.1, 1.),
            gpui::hsla(0.6, 0.5, 0.5, 1.),
        )
    }

    #[test]
    fn highlight_ranges_are_sorted_non_overlapping_and_on_char_boundaries() {
        // Multi-byte text on both sides of every styled span: byte offsets
        // computed off `char_indices` would land mid-codepoint here.
        let blocks = parse("é **grüß** é `código` é [ünlink](https://example.com) é");
        let Block::Paragraph { spans } = &blocks[0] else {
            panic!("expected paragraph, got {:?}", blocks[0]);
        };
        let inline = build(spans);

        let mut last = 0;
        for (range, _) in &inline.highlights {
            assert!(range.start >= last, "ranges out of order: {inline:?}");
            assert!(range.start < range.end);
            assert!(inline.text.is_char_boundary(range.start));
            assert!(inline.text.is_char_boundary(range.end));
            last = range.end;
        }
        assert_eq!(inline.highlights.len(), 3, "bold, code and link");
    }

    #[test]
    fn code_spans_get_a_chip_and_a_font_override() {
        let inline = build(&[span(
            "cargo test",
            Emphasis {
                code: true,
                ..Default::default()
            },
            None,
        )]);
        assert_eq!(inline.fonts.len(), 1);
        assert_eq!(inline.fonts[0].1.as_ref(), "Menlo");
        assert!(inline.highlights[0].1.background_color.is_some());
        assert_eq!(inline.text.as_ref(), "cargo test");
    }

    #[test]
    fn links_produce_click_ranges_over_their_own_text() {
        let spans = vec![
            span("See ", Emphasis::default(), None),
            span(
                "the issue",
                Emphasis::default(),
                Some("https://example.com"),
            ),
            span(" now", Emphasis::default(), None),
        ];
        let inline = build(&spans);
        assert_eq!(inline.links.len(), 1);
        let (range, url) = &inline.links[0];
        assert_eq!(&inline.text[range.clone()], "the issue");
        assert_eq!(url.as_ref(), "https://example.com");
        assert!(inline.highlights[0].1.underline.is_some());
    }

    #[test]
    fn plain_text_produces_no_highlights_at_all() {
        let inline = build(&[span("just words", Emphasis::default(), None)]);
        assert!(inline.highlights.is_empty());
        assert!(inline.fonts.is_empty());
        assert!(inline.links.is_empty());
    }

    #[test]
    fn markers_render_with_a_continuation_that_holds_the_column() {
        assert_eq!(marker_label(Marker::Bullet).as_ref(), "•");
        assert_eq!(marker_label(Marker::Ordered(12)).as_ref(), "12.");
        assert_eq!(marker_label(Marker::Task(true)).as_ref(), "☑");
        assert_eq!(marker_label(Marker::Task(false)).as_ref(), "☐");
        assert_eq!(marker_label(Marker::Continuation).as_ref(), "");
    }
}
