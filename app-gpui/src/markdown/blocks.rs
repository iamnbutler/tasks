//! Source → a flat block model.
//!
//! Pure: no gpui, no theme, no `App`. Everything interesting about markdown
//! happens here, so it can be tested without a window — the scout environment
//! (and CI) has neither a compositor nor a GPU.
//!
//! The model is flat like Zed's `crates/markdown`: one pass over
//! `pulldown_cmark` events produces a `Vec<Block>`, and a list item is a block
//! carrying its own marker and depth rather than a nested tree. Block quotes
//! are the one nesting case, because their frame nests visually.
//!
//! Inline content is always `Vec<Span>`, never a pre-flattened string. That is
//! the whole point: a link in the middle of a sentence has to stay in the
//! middle of the sentence.

use pulldown_cmark::{
    Alignment as CmarkAlignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};

/// Inline styling flags. `code` is ours; the other three mirror gpuikit's
/// `InlineStyle` so a span converts to a `HighlightStyle` the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub code: bool,
}

impl Emphasis {
    pub fn is_plain(&self) -> bool {
        !self.bold && !self.italic && !self.strikethrough && !self.code
    }
}

/// A run of inline text sharing one style and (optionally) one link target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub emphasis: Emphasis,
    /// Destination URL when this run is part of a link (or an image, which
    /// renders as its alt text linked to the source).
    pub link: Option<String>,
}

impl Span {
    #[cfg(test)]
    fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            emphasis: Emphasis::default(),
            link: None,
        }
    }
}

/// What sits in a list item's marker column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    Ordered(u64),
    Task(bool),
    /// A second paragraph inside one item: the marker column stays, empty, so
    /// the continuation lines up under the first and numbering doesn't
    /// restart.
    Continuation,
}

/// Column alignment for tables — our own so the renderer never imports
/// `pulldown_cmark`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Default,
    Left,
    Center,
    Right,
}

/// One table cell's inline content.
pub type Cell = Vec<Span>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Heading {
        /// 1–6.
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    ListItem {
        marker: Marker,
        /// Nesting depth, 0 for a top-level list.
        depth: usize,
        spans: Vec<Span>,
    },
    Code {
        language: Option<String>,
        text: String,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Table {
        alignments: Vec<Align>,
        header: Vec<Cell>,
        rows: Vec<Vec<Cell>>,
    },
    Rule,
}

/// Parse markdown source into the block model.
pub fn parse(source: &str) -> Vec<Block> {
    Builder::new().run(source)
}

/// A block-collecting scope. There is always one; a block quote pushes
/// another, and popping it produces a [`Block::Quote`].
struct Frame {
    blocks: Vec<Block>,
    /// `lists.len()` when this frame opened. A paragraph belongs to a list
    /// item only if the list started *inside* this frame — otherwise a quote
    /// nested in a list item would swallow the item's marker.
    list_base: usize,
}

struct ListState {
    ordered: bool,
    index: u64,
}

struct ImageState {
    url: String,
    alt: String,
}

struct Builder {
    frames: Vec<Frame>,
    spans: Vec<Span>,
    emphasis: Emphasis,
    link: Option<String>,
    image: Option<ImageState>,
    lists: Vec<ListState>,
    /// The marker owed to the next flush inside the current list item. Taken
    /// on first use, so the item's second paragraph gets a continuation.
    pending_marker: Option<Marker>,
    heading: Option<u8>,
    code: Option<(Option<String>, String)>,
    table: Option<TableState>,
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Align>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    in_head: bool,
}

impl Builder {
    fn new() -> Self {
        Self {
            frames: vec![Frame {
                blocks: Vec::new(),
                list_base: 0,
            }],
            spans: Vec::new(),
            emphasis: Emphasis::default(),
            link: None,
            image: None,
            lists: Vec::new(),
            pending_marker: None,
            heading: None,
            code: None,
            table: None,
        }
    }

    fn run(mut self, source: &str) -> Vec<Block> {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_GFM;

        for event in Parser::new_ext(source, options) {
            self.event(event);
        }

        // Unterminated constructs: pulldown closes its own tags at EOF, but an
        // unwound state costs nothing to guarantee and means malformed input
        // can never drop the tail of a document.
        self.flush_text();
        while self.frames.len() > 1 {
            self.pop_quote();
        }
        self.frames
            .pop()
            .map(|frame| frame.blocks)
            .unwrap_or_default()
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, buffer)) = &mut self.code {
                    buffer.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            // Inline code, with the backticks gone — the chip does that job.
            Event::Code(code) => {
                let outer = self.emphasis;
                self.emphasis.code = true;
                self.push_text(&code);
                self.emphasis = outer;
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_text("\n"),
            Event::Rule => {
                self.flush_text();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                self.pending_marker = Some(Marker::Task(checked));
            }
            // HTML is dropped, except `<br>`: dropping it visibly mangles
            // GitHub issue bodies, which use it for line breaks in tables and
            // tight lists.
            Event::Html(html) | Event::InlineHtml(html) => {
                if is_line_break(&html) {
                    self.push_text("\n");
                }
            }
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => self.heading = Some(level as u8),
            Tag::BlockQuote(_) => {
                self.flush_text();
                self.pending_marker = None;
                self.frames.push(Frame {
                    blocks: Vec::new(),
                    list_base: self.lists.len(),
                });
            }
            Tag::CodeBlock(kind) => {
                self.flush_text();
                // A fence inside a list item becomes a top-level code block;
                // dropping the owed marker keeps an empty bullet row from
                // appearing above it.
                self.pending_marker = None;
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let language = info.split_whitespace().next().unwrap_or("");
                        (!language.is_empty()).then(|| language.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                // A tight parent item's own text lands before its sublist, and
                // must be flushed at the parent's depth.
                self.flush_text();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    index: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.pending_marker = Some(match self.lists.last_mut() {
                    Some(list) if list.ordered => {
                        let marker = Marker::Ordered(list.index);
                        list.index += 1;
                        marker
                    }
                    _ => Marker::Bullet,
                });
            }
            Tag::Emphasis => self.emphasis.italic = true,
            Tag::Strong => self.emphasis.bold = true,
            Tag::Strikethrough => self.emphasis.strikethrough = true,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImageState {
                    url: dest_url.to_string(),
                    alt: String::new(),
                })
            }
            Tag::Table(alignments) => {
                self.flush_text();
                self.table = Some(TableState {
                    alignments: alignments.into_iter().map(align).collect(),
                    ..Default::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                    table.row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => self.spans.clear(),
            Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::HtmlBlock => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_text(),
            TagEnd::Heading(_) => self.flush_text(),
            TagEnd::BlockQuote(_) => {
                self.flush_text();
                if self.frames.len() > 1 {
                    self.pop_quote();
                }
            }
            TagEnd::CodeBlock => {
                if let Some((language, mut text)) = self.code.take() {
                    // Fences carry a trailing newline that would render as a
                    // blank last line inside the chrome.
                    while text.ends_with('\n') {
                        text.pop();
                    }
                    self.push_block(Block::Code { language, text });
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.pending_marker = None;
            }
            TagEnd::Item => {
                self.flush_text();
                // An item whose content produced no block at all (`-` on its
                // own line) still gets its row, so the list keeps its shape.
                if let Some(marker) = self.pending_marker.take() {
                    let depth = self.lists.len().saturating_sub(1);
                    self.push_block(Block::ListItem {
                        marker,
                        depth,
                        spans: Vec::new(),
                    });
                }
            }
            TagEnd::Emphasis => self.emphasis.italic = false,
            TagEnd::Strong => self.emphasis.bold = false,
            TagEnd::Strikethrough => self.emphasis.strikethrough = false,
            TagEnd::Link => self.link = None,
            TagEnd::Image => self.flush_image(),
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Block::Table {
                        alignments: table.alignments,
                        header: table.header,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                    table.header = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::HtmlBlock => {}
        }
    }

    /// Append text to the current inline run, merging with the previous span
    /// when style and link agree.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Between `Start(Image)` and `End(Image)` the text *is* the alt text.
        if let Some(image) = &mut self.image {
            image.alt.push_str(text);
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            if last.emphasis == self.emphasis && last.link == self.link {
                last.text.push_str(text);
                return;
            }
        }
        self.spans.push(Span {
            text: text.to_string(),
            emphasis: self.emphasis,
            link: self.link.clone(),
        });
    }

    /// Images are not fetched: an image becomes its alt text (or the source's
    /// file name) linked to the source, which the browser can open.
    fn flush_image(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        let label = if image.alt.trim().is_empty() {
            file_name(&image.url)
        } else {
            image.alt.clone()
        };
        if label.is_empty() {
            return;
        }
        self.spans.push(Span {
            text: label,
            emphasis: self.emphasis,
            link: Some(image.url),
        });
    }

    /// Emit the accumulated inline run as whatever block context it sits in.
    fn flush_text(&mut self) {
        if self.spans.iter().all(|span| span.text.is_empty()) {
            self.spans.clear();
            // A heading with no text still closes the heading context.
            self.heading = None;
            return;
        }
        // Table cells flush at `TagEnd::TableCell`, not here.
        if self.table.is_some() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);

        if let Some(level) = self.heading.take() {
            self.push_block(Block::Heading { level, spans });
            return;
        }

        let list_base = self.frames.last().map(|f| f.list_base).unwrap_or(0);
        if self.lists.len() > list_base {
            let marker = self.pending_marker.take().unwrap_or(Marker::Continuation);
            let depth = self.lists.len() - list_base - 1;
            self.push_block(Block::ListItem {
                marker,
                depth,
                spans,
            });
            return;
        }

        self.push_block(Block::Paragraph { spans });
    }

    fn push_block(&mut self, block: Block) {
        if let Some(frame) = self.frames.last_mut() {
            frame.blocks.push(block);
        }
    }

    fn pop_quote(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        self.push_block(Block::Quote {
            blocks: frame.blocks,
        });
    }
}

fn align(alignment: CmarkAlignment) -> Align {
    match alignment {
        CmarkAlignment::None => Align::Default,
        CmarkAlignment::Left => Align::Left,
        CmarkAlignment::Center => Align::Center,
        CmarkAlignment::Right => Align::Right,
    }
}

fn is_line_break(html: &str) -> bool {
    let html = html.trim();
    html.eq_ignore_ascii_case("<br>")
        || html.eq_ignore_ascii_case("<br/>")
        || html.eq_ignore_ascii_case("<br />")
}

/// The last path segment of a URL, query and fragment stripped.
fn file_name(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(block: &Block) -> &[Span] {
        match block {
            Block::Heading { spans, .. }
            | Block::Paragraph { spans }
            | Block::ListItem { spans, .. } => spans,
            _ => panic!("block carries no inline spans: {block:?}"),
        }
    }

    fn text_of(block: &Block) -> String {
        spans_of(block).iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn link_stays_inline() {
        let blocks = parse("See [the issue](https://example.com/822) for context.");
        assert_eq!(blocks.len(), 1, "one paragraph, not three: {blocks:?}");
        let spans = spans_of(&blocks[0]);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0], Span::plain("See "));
        assert_eq!(spans[1].text, "the issue");
        assert_eq!(spans[1].link.as_deref(), Some("https://example.com/822"));
        assert_eq!(spans[2], Span::plain(" for context."));
    }

    #[test]
    fn inline_code_drops_its_backticks() {
        let blocks = parse("Run `cargo test` first.");
        let spans = spans_of(&blocks[0]);
        assert_eq!(spans[1].text, "cargo test");
        assert!(spans[1].emphasis.code);
        assert_eq!(text_of(&blocks[0]), "Run cargo test first.");
    }

    #[test]
    fn emphasis_nests() {
        let blocks = parse("**bold _both_** ~~gone~~");
        let spans = spans_of(&blocks[0]);
        assert_eq!(spans[0].text, "bold ");
        assert!(spans[0].emphasis.bold && !spans[0].emphasis.italic);
        assert_eq!(spans[1].text, "both");
        assert!(spans[1].emphasis.bold && spans[1].emphasis.italic);
        assert!(spans.last().unwrap().emphasis.strikethrough);
    }

    #[test]
    fn adjacent_runs_of_one_style_merge() {
        let blocks = parse("plain plain plain");
        assert_eq!(spans_of(&blocks[0]).len(), 1);
    }

    #[test]
    fn headings_carry_their_level() {
        let blocks = parse("# One\n\n### Three\n\n###### Six");
        let levels: Vec<u8> = blocks
            .iter()
            .map(|block| match block {
                Block::Heading { level, .. } => *level,
                other => panic!("expected heading, got {other:?}"),
            })
            .collect();
        assert_eq!(levels, vec![1, 3, 6]);
        assert_eq!(text_of(&blocks[0]), "One");
    }

    #[test]
    fn bullets_and_ordered_items_get_markers() {
        let blocks = parse("- one\n- two\n\n1. first\n2. second");
        let markers: Vec<Marker> = blocks
            .iter()
            .map(|block| match block {
                Block::ListItem { marker, .. } => *marker,
                other => panic!("expected list item, got {other:?}"),
            })
            .collect();
        assert_eq!(
            markers,
            vec![
                Marker::Bullet,
                Marker::Bullet,
                Marker::Ordered(1),
                Marker::Ordered(2)
            ]
        );
    }

    #[test]
    fn ordered_lists_honour_their_start() {
        let blocks = parse("7. seven\n8. eight");
        assert!(matches!(
            blocks[0],
            Block::ListItem {
                marker: Marker::Ordered(7),
                ..
            }
        ));
        assert!(matches!(
            blocks[1],
            Block::ListItem {
                marker: Marker::Ordered(8),
                ..
            }
        ));
    }

    #[test]
    fn nested_lists_get_depth() {
        let blocks = parse("- outer\n    - inner\n        - innermost\n- outer again");
        let depths: Vec<usize> = blocks
            .iter()
            .map(|block| match block {
                Block::ListItem { depth, .. } => *depth,
                other => panic!("expected list item, got {other:?}"),
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 2, 0]);
        assert_eq!(text_of(&blocks[0]), "outer");
        assert_eq!(text_of(&blocks[2]), "innermost");
    }

    #[test]
    fn task_markers_replace_the_bullet() {
        let blocks = parse("- [x] done\n- [ ] todo");
        assert!(matches!(
            blocks[0],
            Block::ListItem {
                marker: Marker::Task(true),
                ..
            }
        ));
        assert!(matches!(
            blocks[1],
            Block::ListItem {
                marker: Marker::Task(false),
                ..
            }
        ));
        assert_eq!(text_of(&blocks[0]), "done");
    }

    #[test]
    fn loose_list_continuation_keeps_the_marker_column() {
        let blocks = parse("1. first para\n\n   second para\n\n2. next item");
        assert!(matches!(
            blocks[0],
            Block::ListItem {
                marker: Marker::Ordered(1),
                ..
            }
        ));
        assert!(
            matches!(
                blocks[1],
                Block::ListItem {
                    marker: Marker::Continuation,
                    ..
                }
            ),
            "continuation must not restart numbering: {:?}",
            blocks[1]
        );
        assert!(matches!(
            blocks[2],
            Block::ListItem {
                marker: Marker::Ordered(2),
                ..
            }
        ));
    }

    #[test]
    fn fences_keep_their_language_and_lose_their_trailing_newline() {
        let blocks = parse("```rust\nfn main() {}\n```");
        match &blocks[0] {
            Block::Code { language, text } => {
                assert_eq!(language.as_deref(), Some("rust"));
                assert_eq!(text, "fn main() {}");
            }
            other => panic!("expected code, got {other:?}"),
        }
    }

    #[test]
    fn fences_without_an_info_string_have_no_language() {
        let blocks = parse("```\nplain\n```");
        assert!(matches!(&blocks[0], Block::Code { language: None, .. }));
    }

    #[test]
    fn an_unterminated_fence_still_yields_its_body() {
        let blocks = parse("before\n\n```sh\nmake test\n");
        assert_eq!(text_of(&blocks[0]), "before");
        match &blocks[1] {
            Block::Code { language, text } => {
                assert_eq!(language.as_deref(), Some("sh"));
                assert_eq!(text, "make test");
            }
            other => panic!("expected code, got {other:?}"),
        }
    }

    #[test]
    fn a_fence_inside_a_list_item_becomes_a_top_level_block() {
        let blocks = parse("- item\n\n  ```\n  code\n  ```\n");
        assert!(matches!(blocks[0], Block::ListItem { .. }));
        assert!(matches!(&blocks[1], Block::Code { .. }));
        assert_eq!(blocks.len(), 2, "no empty marker row: {blocks:?}");
    }

    #[test]
    fn quotes_nest() {
        let blocks = parse("> outer\n>\n> > inner\n");
        let Block::Quote { blocks: outer } = &blocks[0] else {
            panic!("expected quote, got {:?}", blocks[0]);
        };
        assert_eq!(text_of(&outer[0]), "outer");
        let Block::Quote { blocks: inner } = &outer[1] else {
            panic!("expected nested quote, got {:?}", outer[1]);
        };
        assert_eq!(text_of(&inner[0]), "inner");
    }

    #[test]
    fn a_quote_inside_a_list_item_does_not_steal_the_marker() {
        let blocks = parse("- item\n\n  > quoted\n");
        assert!(matches!(blocks[0], Block::ListItem { .. }));
        let Block::Quote { blocks: quoted } = &blocks[1] else {
            panic!("expected quote, got {:?}", blocks[1]);
        };
        assert!(
            matches!(quoted[0], Block::Paragraph { .. }),
            "quoted content is a paragraph, not a list item: {:?}",
            quoted[0]
        );
    }

    #[test]
    fn tables_split_header_from_body() {
        let blocks = parse("| a | b |\n| --- | ---: |\n| 1 | 2 |\n| 3 | 4 |");
        match &blocks[0] {
            Block::Table {
                alignments,
                header,
                rows,
            } => {
                assert_eq!(alignments, &vec![Align::Default, Align::Right]);
                assert_eq!(header.len(), 2);
                assert_eq!(header[0][0].text, "a");
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[1][1][0].text, "4");
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn table_cells_keep_their_inline_styling() {
        let blocks = parse("| a |\n| --- |\n| `code` |");
        let Block::Table { rows, .. } = &blocks[0] else {
            panic!("expected table, got {:?}", blocks[0]);
        };
        assert!(rows[0][0][0].emphasis.code);
        assert_eq!(rows[0][0][0].text, "code");
    }

    #[test]
    fn soft_breaks_join_and_hard_breaks_split() {
        let joined = parse("one\ntwo");
        assert_eq!(text_of(&joined[0]), "one two");
        let split = parse("one  \ntwo");
        assert_eq!(text_of(&split[0]), "one\ntwo");
    }

    #[test]
    fn br_survives_and_other_html_is_dropped() {
        let blocks = parse("one<br>two<span>three</span>");
        assert_eq!(text_of(&blocks[0]), "one\ntwothree");
        assert_eq!(parse("<details><summary>hi</summary></details>").len(), 0);
    }

    #[test]
    fn images_render_as_alt_text_linked_to_their_source() {
        let blocks = parse("![a screenshot](https://example.com/img/shot.png)");
        let spans = spans_of(&blocks[0]);
        assert_eq!(spans[0].text, "a screenshot");
        assert_eq!(
            spans[0].link.as_deref(),
            Some("https://example.com/img/shot.png")
        );
    }

    #[test]
    fn an_image_without_alt_text_falls_back_to_its_file_name() {
        let blocks = parse("![](https://example.com/img/shot.png?raw=1)");
        assert_eq!(spans_of(&blocks[0])[0].text, "shot.png");
    }

    #[test]
    fn rules_are_blocks() {
        let blocks = parse("above\n\n---\n\nbelow");
        assert!(matches!(blocks[1], Block::Rule));
    }

    #[test]
    fn empty_and_whitespace_source_produce_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("   \n\n  ").is_empty());
    }

    #[test]
    fn unterminated_emphasis_stays_literal() {
        let blocks = parse("**not closed");
        let spans = spans_of(&blocks[0]);
        assert_eq!(spans[0].text, "**not closed");
        assert!(spans[0].emphasis.is_plain());
    }
}
