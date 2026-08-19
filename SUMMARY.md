# Say plainly that Tasks is unsafe (#984)

Nothing in this repository told a reader what running Tasks does to their
machine or their GitHub account. This adds the words, in the register #999
sets — VLC and 7-Zip, not a product and not a EULA — in the two surfaces that
exist today. `README.md` grows a `## Read this first` section between the
opening paragraph and `## The idea`: four bullets, each naming an act that is
checkable in the tree, and a closing paragraph that says the server boots
paused, says where to point it, and carries the no-warranty sentence in plain
English rather than in imitation legalese. The app gets
`app-gpui/src/disclaimer.rs`, one module holding `HEADLINE`, `SUMMARY`,
`PIPELINE_CAUTION`, `PLAY_TOOLTIP` and `README_POINTER`, which the About
window (grown 320×200 → 380×300, left-aligned and width-bounded, because
centred prose past one line reads as a splash screen), the Server window's
pipeline control and the Play button's tooltip all read from — three surfaces
read at three different moments, which is exactly how three inline strings
become three different claims.

The distinction worth not flattening, and the one the issue's own bullet list
got wrong, is carried in both surfaces: **the agents are confined and the
server is not.** A Scout's or Builder's lease reaches Anthropic and reads the
single repository it was dispatched for (`Scopes::AGENT`, `broker.rs`) and
cannot push; the push, the merge and the close are the server's own acts,
under its own credential, on an agent's say-so. "Agents can push" is wrong in
the direction that makes a disclaimer ignorable. Prose is the deliverable and
prose is the one deliverable nothing else in this tree can keep honest, so
`crates/tasks/tests/disclaimer.rs` holds three guards over **both** bodies of
copy: the section sits above the architecture, every act is still named in the
README *and* in the app's constants, and neither has picked up a hedge. All
four failure modes were checked by breaking them one at a time — softening the
app's copy alone, softening the README's alone, relocating the section below
`## The idea`, and adding "at your own risk" — and each turns the suite red.

## Regions touched

- **`README.md`** — a single 41-line insertion between line 6 (the end of the
  opening paragraph) and `## The idea`. One hunk, additions only: no reflow,
  no reordering, no formatter over the file. The heading is exactly
  `## Read this first`, so #994's build can reference it.
- **`app-gpui/src/server_window.rs`** — one addition inside
  `render_pipeline`, after the existing `when_some(mode_error)` child: the
  muted `PIPELINE_CAUTION` line in the same left-padded slot. Nothing else in
  the file.
- **`app-gpui/src/workspace.rs`** — one line, the Play button's tooltip.
- **`app-gpui/src/main.rs`** — `mod disclaimer;`.
- **`app-gpui/src/about.rs`**, **`app-gpui/src/disclaimer.rs`**,
  **`crates/tasks/tests/disclaimer.rs`** — as specced.

## One fact the spec had, that the tree no longer has

**#985 landed** (`crates/tasks/src/loopback.rs`, commit 358cb39), so the
spec's third bullet — "an unauthenticated local API any web page can post to
until #985 lands" — is no longer true as written, and the spec's own
instruction was to check rather than copy. The bullet now says what is true:
the local API has no authentication, so anything on your machine that can
reach port 4800 can drive the pipeline and it is recorded as you; web pages
are refused, but that guard is about pages, not about processes. Two
consequences, both deliberate:

- The HTML comment dating the bullet to #985, and the guard assertion that
  `#985` is still mentioned, are **not** included. Their purpose was to make a
  dated claim impossible to leave standing after the fix landed; the fix has
  landed and I am the one writing the paragraph after it. Pinning the issue
  number now would only go red on someone deleting a historical reference,
  which is the kind of guard people delete. The act is pinned instead
  (`no authentication`), in both surfaces.
- The guard does not assert the residual gap the loopback module names (a
  cross-site subresource `GET`). The copy neither claims nor denies it; the
  module documents it, and overstating the API's exposure in a disclaimer is
  the same defect as understating it.

## Review feedback

1. **Extend the drift guard across the repository boundary, or defend the
   gap.** Done, not declined. `the_disclaimer_names_what_the_system_actually_does`
   now reads `README.md` *and* `app-gpui/src/disclaimer.rs` and asserts every
   act in `ACTS` appears in both, from one list, with a failure message that
   names which surface fell behind. Verified by softening each side alone and
   watching the other side's assertion fail. One thing this forced that is
   worth flagging: reading the whole `disclaimer.rs` file does **not** work —
   the module's own unit tests contain the act phrases as literals, so a
   whole-file `contains` passes on a module whose *constants* have been
   softened. That is this test's own failure mode, one level up from the HTML
   comment the spec warned about, and it is why `app_copy()` extracts only
   `pub const` values. Caught because the falsification was run rather than
   assumed.
2. **Name the hedge denylist in full, and be honest about what it can do.**
   Done. `HEDGES` is eleven named phrases and its doc comment says plainly
   that it is a proxy, that prose can be hedged into uselessness without using
   any of them, that the positive test is the one carrying the weight, and
   that a green suite is not evidence the copy is still blunt.
3. **`env!("CARGO_MANIFEST_DIR")` rather than `../../README.md`.** Done; both
   files are resolved from it, and the module doc says why.
4. **Report the verification precisely, including what did not run.** Done,
   below — and the news is better than the spec's: `make app-test` is green
   here. The spec reported it dying at the `tasks-menubar` test binary's link
   step in a 6 GB Scout VM; this is an 8 GB Builder VM and both binaries
   linked. Both targets are named separately so nobody has to infer which
   ran.
5. **Sequencing — expect these files to have moved.** Acknowledged, no code
   change. #967 landed (PR #1021) and is in this base, so `server_window.rs`
   was uncontended; the README was untouched by anything landed since, so the
   insertion applied cleanly. Regions are listed above so merge order can be
   checked without diffing.

## Directions

- **Account for the five required changes.** Done, above.
- **`README.md` is contended: insert `## Read this first` at the top, touch
  nothing else.** Done — one insertion hunk, additions only, exact heading,
  between the opening paragraph and `## The idea`. `cargo fmt` was not run
  over any Markdown and nothing below the section was reflowed.
- **`server_window.rs`: keep it to the caution and the tooltip.** Done — one
  added child in `render_pipeline`, and the tooltip (which is in
  `workspace.rs`, not `server_window.rs`).
- **Say which regions were touched.** Done, above.
- **The previous run died to a host suspend; nothing was judged.** Noted; this
  is a fresh implementation, not a continuation. No `NOTES.md` was present and
  none is committed.

## Not done, deliberately

The play-confirmation modal stays unimplemented — it wants #1013's modal
layer, which is not in this tree (`Modal` appears in `app-gpui/src` only in
doc comments). When it lands it should read `disclaimer::HEADLINE` and
`disclaimer::PIPELINE_CAUTION` rather than writing new words. The site (#995)
does not exist; the README's section is canonical and the site inherits it
verbatim rather than paraphrasing. #983 has since landed, and `LICENSE` now
carries the legal no-warranty text — the README's plain-English sentence
stays, doing a different job, and points at it.

Verification: PASSED — `make test` (959 passed, 0 failed, 7 leaky as documented; doctests green), `make app-test` (213 + 35 passed — the full target, including the `tasks-menubar` test binary the spec could not link), `cargo fmt --all --check` and `cargo fmt --check` in `app-gpui`, `cargo clippy --workspace --all-targets` (clean) and `cargo clippy --all-targets` in `app-gpui` (five warnings, all pre-existing in `src/bin/tasks-menubar/popup.rs`, untouched here).
