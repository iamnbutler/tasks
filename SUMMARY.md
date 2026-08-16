# Adopt gpuikit 0.7, pin gpui exactly, commit app-gpui's lockfile — and scope markdown element ids per document

gpuikit 0.7.0 is on crates.io, and the rev app-gpui was pinned to (`1d9aaf3`,
the PR #122 branch commit) is an ancestor of the `v0.7.0` tag — so the bump is
a fast-forward of the pin and gives nothing back. app-gpui moves to
`gpuikit = "0.7"`, its two gpui requirements become exact (`=1.14.2`, which is
what gpuikit itself asks for, so the pin unifies with it rather than fighting
it and a future gpuikit needing 1.15 becomes a loud resolution error instead of
a silent bump), and `Cargo.lock` is committed with a `!/app-gpui/Cargo.lock`
negation under the root `.gitignore`'s blanket `Cargo.lock` line — which is why
this crate went without one. The negation is load-bearing rather than
cosmetic: `git add -f` would commit the file once and then hide every later
regeneration from `git status`. The root `Cargo.toml` already described
app-gpui as building "standalone (own `Cargo.lock`)", so this restores a stated
invariant rather than inventing one. There is no `git+` line left in the
lockfile; the unpin is total.

On the two markdown defects the answer is split, and the split is the point.
The inline-link and inline-code fixes are real — verified by reading 0.7.0's
source rather than its changelog (`flush_link` no longer exists). But **#861's
crash stops without its root cause being fixed**: 0.7 still mints text-run ids
(`md-run-1`, `md-run-2`, …) from a counter it restarts on every render, and now
does so for *every* text run rather than only links — so where 0.6 needed two
documents containing links to collide, 0.7 collides on any two documents
containing text at all. What makes it stop panicking is incidental: the
selection rewrite replaced gpui's `InteractiveText` (which reports an a11y role
unconditionally) with gpuikit's own `SelectableText` (which reports none), so
the duplicate ids never reach the a11y tree's uniqueness check. The collision
is inert, not absent, and re-arms the moment upstream gives `SelectableText` a
role back or starts keying element state off the id. So `markdown_block` now
wraps its element in a `div().id(("markdown", entity.entity_id()))`, giving each
document a distinct id ancestor and making the app immune either way. It is
keyed on the entity id because `MarkdownCache` already hands out one stable
entity per key, so the id is unique per document *and* stable across frames;
the wrapper costs nothing (an id with no listeners, hover style or cursor adds
no hitbox, and `w_full` around an already-`w_full` root changes no layout) and
needs no signature or call-site changes beyond the return type widening to
`impl IntoElement`. #861 stays open, retargeted at upstream.

One thing worth flagging for whoever regenerates the lockfile, and the one
place this diverges from the spec: gpui's crates are published as a lockstep
family, but the 1.14.2 core requires only `^1.14.2` of its own support crates
(`gpui-util`, `gpui-shared-string`, `collections`, …). Since the spec was
written, 1.15.0 of those was published, so a plain `cargo generate-lockfile`
now pairs a 1.14.2 core with 1.15.0 support crates — a combination upstream
never ships or tests. The lockfile holds the whole `*-gpui-unofficial` family
at 1.14.2 on purpose, and `app-gpui/README.md` records both that and the
exact-pin rationale where someone regenerating it will read them.
