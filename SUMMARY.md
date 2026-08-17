Three app-gpui changes, and two of the three issues turned out to be mostly
already-fixed — so the deliverable in both cases is the record, not the code.

**#922 — dev-profile optimization.** `[profile.dev.package."*"] opt-level = 2`
does not land in `app-gpui/Cargo.toml`. Cargo reads profiles only from the root
manifest of the build being run, and app-gpui is deliberately not a `tasks`
workspace member, so it is its own root: no workspace profile reaches it, and
neither does gpuikit's own stanza (iamnbutler/gpuikit#140) — whose changelog
will nonetheless tell a reader here that the problem is fixed. The setting was
measured, and every measured effect is a cost: cold build 182s → 644s, debug
binary 592 MB → 761 MB, `target/` 3.8G → 8.3G (Cargo keeps both artifact sets),
and the edit loop does not move. The benefit is smoother frames, which can only
be seen on a Mac and has never been measured, and it accrues to one person
running `cargo run` while the 3.5× cold build is paid by every fresh clone,
including every agent VM that touches this crate. So it is documented as an
`app-gpui/.cargo/config.toml` opt-in, which Cargo honours identically (570 of
572 rustc invocations in a cold build carry `-C opt-level=2` with that file
present), with the measured table, the scope rule, three gotchas, and a
Mac-only before/after protocol that insists on `cargo run` — `make app` and
`make run` are `--release` and cannot see a `[profile.dev]` setting at all,
which is the most likely way the open question gets answered wrongly.
`Cargo.toml` gets a comment where the stanza would have gone, since that is the
file the next person edits, and `.gitignore` gets `/.cargo/` so a local opt-in
never shows in `git status`.

**#861 — the a11y panic.** The collision is genuinely gone, and it was not
fixed here: the issue is written against gpuikit `1d9aaf3` / 0.6.0, and the pin
moved to `b28732f` for streaming (#927), which carries gpuikit #133 (markdown
run ids scoped per document) and #145 (the crate-wide element-id audit). What
was left is that this app's own defence — the wrapper id from #882 — had a doc
comment claiming a collision upstream has since fixed, so the next reader
learns the wrapper is redundant. The id becomes a named, unit-tested
`block_element_id(EntityId) -> ElementId`, byte-for-byte what
`.id(("markdown", entity_id))` already produced, mirroring gpuikit's own
`element_id::for_entity`; the comment now states what is still true (run ids
are unique only *within* a document) and keeps the history that is the argument
for the wrapper — gpuikit 0.7.0 still collided and was merely *inert*, because
its runs carried no a11y role, and #133 gave them roles back in the same commit
that scoped the ids. README ▸ Dependencies prices the documented revert to
`gpuikit = "0.7"`, which hands back the unscoped ids. Nothing about what the
app draws changes.

**#864 — markdown streaming, highlighting, selection, pulldown-cmark.** Three
of the four items already exist at the current pin (`append` is implemented and
already wired into `MarkdownCache`; pulldown-cmark is already 0.13.4), so the
work is wiring. The `editor` feature goes on — wanted for its syntect bridge,
not the editor widget — with `init_code_highlighting` forwarded through
`components::markdown` and called at startup, where it is order-independent
unlike the keybinding calls around it. It ships with its cost recorded rather
than discovered later: gpuikit's highlight cache keys on the whole block text,
so a fence arriving through `append` re-highlights the block-so-far on every
delta, per frame — a 400-line fence is 15.2 ms settled and 5.32 s streamed,
worst delta ~16.3 ms — and one long streamed fence evicts every settled block
from the 256-entry cache. There is no app-side lever, and the reading surfaces
win outright. ⌘C routes through gpuikit's own `input::Copy` action, which is
the design and not a shortcut: the Edit menu's Copy is a `MenuItem::os_action`,
so AppKit answers ⌘C from the menu bar and an action of our own bound to
`cmd-c` would fire on no platform at all; registered on the workspace root so a
focused composer still takes it first. The one piece of genuinely new
engineering is cross-document arbitration — gpuikit clears selections only in
documents it painted this frame, so two documents can hold one at once with
nothing upstream able to arbitrate, because nothing upstream knows they share a
window. `MarkdownCache` now names an active key, resolves it once per frame
before anything paints, clears the losers, and reads `selected_text` from that
key alone instead of scanning a `HashMap`; the decision is a pure function with
five tests, including the settled frame that makes the per-frame clear
idempotent.

Two things cannot be verified on Linux and want a `make app` run on a Mac:
that ⌘C actually arrives as `input::Copy` through the Edit menu's `os_action`,
and that highlighted code blocks read correctly against the app's theme. The
frame-rate question behind #922 is the same shape and now has a documented
procedure and a place to record the answer. `src/commands.rs` is also
reformatted — it has been failing `cargo fmt --check` since #918 shortened its
import list, unrelated to any of the above and included only so the check is
worth running. `app-gpui/Cargo.lock` gains syntect and onig from the `editor`
feature; the `*-gpui-unofficial` family is still pinned at 1.14.2 and
`grep git+ Cargo.lock` is still one line.

Verification: PASSED — `make app-test` (184 passed, 0 failed), `make app-check`, `cargo fmt --check` and `cargo clippy --all-targets` in `app-gpui`, plus `make test` for the workspace (695 passed, 0 failed, 7 leaky as `.config/nextest.toml` expects; doctests green). The only warning anywhere is the pre-existing `proc-macro-error2` future-incompat one already on `main`. The workspace suite cannot see this diff — nothing in `crates/` is touched and `app-gpui` is not a workspace member — and no claim here rests on a running window: `make app` is macOS-only.
