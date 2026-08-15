# app-gpui renders markdown in chat bubbles and the inspector

Agent turns, specs and issue bodies were rendered as plain strings — one
uniform text run, with backticks and `**` shown literally. This adds a
markdown engine to app-gpui (`src/markdown/`) and wires it into the two
surfaces that carry markdown today: the orchestrator chat bubble
(`workspace.rs::render_chat`) and the inspector's pending spec and task body
(`sections/detail.rs`). `blocks.rs` parses source into a flat block model
(`Heading | Paragraph | ListItem | Code | Quote | Table | Rule`, with inline
content always a `Vec<Span>`); it is pure — no gpui, no theme, no `App` — so
the interesting cases are testable without a window. `render.rs` renders one
`StyledText` per block with highlight ranges and font-family overrides, which
is what keeps emphasis, inline code and **links inline** instead of splitting
the sentence around them into stacked blocks. `mod.rs` holds `MarkdownStore`,
a gpui `Global` caching parses by namespaced key (`chat:12`, `spec:…`,
`task-body:…`) and LRU-capped at 128, so parsing happens once per content
change while the element tree is rebuilt per frame. `Event`/`System` chat rows
stay plain text: they are the pipeline's own one-line status sentences, and a
stray underscore in a task title should not restyle them.

The module is shaped as the extension `gpuikit::markdown` wants to grow into
rather than a fork of it: it reuses gpuikit's `MarkdownStyle`/`TextStyle`
vocabulary verbatim, so adopting an improved upstream is a call-site swap. It
exists because 0.6.0 flushes an inline link (and an image) as its own
top-level block, renders inline code with its backticks still in the text, and
reparses on every call from the render path — on this repo's link- and
code-dense content that reads worse than the plain strings it would replace,
and it ships from another repo, so it cannot be fixed from here. One engine
serves both surfaces through two style profiles (`chat_style`, `doc_style`)
that differ only in type scale and in which surface colour a code chip sits on
— chat chips go on `bg()` because the bubble behind them is
`surface_secondary`, doc chips on `surface_secondary` because the pane is
already `bg`. The type scale is gentler than gpuikit's default (whose `h1`
lands at 29px, a shout inside a chat bubble): headings top out at 1.5× body.
`pulldown-cmark` is added at 0.12, held to gpuikit's line so one binary never
holds two parsers. Verified: `cargo test` in `app-gpui` (40 tests — the 7 that
existed plus 33 new), `cargo clippy --all-targets` and `cargo fmt --check`
clean there, and `cargo clippy --workspace --all-targets` plus `cargo test
--workspace` still clean at the root. Not verified: pixels — this was built on
headless Linux with no compositor, so layout and colour are reasoned from the
gpui/gpuikit APIs, not seen. Worth a look on a Mac: a chat turn with a link
mid-sentence, a spec with a fenced code block and a table, and a task body
with a nested list (the `•`/`☑`/`☐` glyphs rely on font fallback out of
Menlo). Images render as alt text linked to their source rather than being
fetched, HTML is dropped except `<br>`, and syntax highlighting, remote
images, selection/copy and streaming `append` are follow-ups, each with a
named seam in the code.
