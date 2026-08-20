# Cut releases, and give the app an icon

Two independent changes, kept as two commits.

**#997 — releases.** `scripts/changelog.sh` writes one CHANGELOG section to
stdout from the commits themselves, and owns the `0.1.<count + 1>` arithmetic
that the Makefile's `PUBLISH_VERSION` calls rather than repeats. Seven targets
compose into `make publish`: refuse unless HEAD is a green `origin/main` with
the tag absent, generate and commit the section, run the **unmodified** #988
signing chain, then tag, push both refs atomically, upload, and re-download the
assets to prove they are there. The walk is deliberately **not**
`--first-parent`: a build merged into another build's branch rather than into
the trunk is reachable from `main` and off its first-parent chain, so
`28c879e` (the Mac app) would have been missing from the very first changelog —
it keeps a commit that is on the trunk *or* is itself a pull-request merge by
subject shape, which is also what puts the housekeeping denylist to work for
the first time. Plus the `CLI_ZIP` rename to `tasks-server-`, the inert-version
comment on `[workspace.package]`, a bootstrap `CHANGELOG.md`, the CLAUDE.md
rule, and `[as built]` notes in both plan docs. No tag is cut here; `make
publish` cannot run until Apple enrollment completes, because it delegates to
`make release`.

**#986 — the app icon.** `Tasks.app` shipped the generic blank because three
things were missing and none of them fails loudly. All three land, plus the
mark in the About window. The `.icns` is not hand-placed binary:
`app-gpui/icon/appicon.py` — stdlib only, since a Builder VM has no `iconutil`,
no `sips` and no image library — emits it and the SVG marks from one set of
geometry constants, so the icon has a source that can be reviewed and
regenerated anywhere, and the Dock and the About window cannot drift. Running
the committed generator reproduced both stated hashes byte for byte
(`AppIcon.icns` 54358 bytes / `6046fb46…`, `AppIcon.svg` 445 bytes /
`c69d824e…`) on Python 3.12.3 — a third platform and zlib, after the Scout's
and the reviewer's. Four guard tests live in the workspace, because every piece
fails silently and no CI runner executes the `check-darwin`-gated recipe that
would notice; they assert structure and never the picture.

## Review feedback

**Spec 1 (#997)**

1. *`--first-parent` drops PRs that landed on another branch first.* Done —
   widened rather than documented. The walk is the full reachable set, keeping
   a commit that is on the first-parent chain or is itself a pull-request merge
   by subject shape. Verified against this repository: the bootstrap section
   now contains "SwiftUI mac app: read-only dashboard over the Tasks API"
   (28c879e), which a first-parent walk omits. The denylist is kept, as you
   argued; `a_pull_request_merged_into_another_branch_is_not_lost` pins it, and
   `housekeeping_is_dropped_by_the_stated_denylist` pins the entries that this
   widening finally puts to work.
2. *Run `cargo test -p tasks --test changelog` first.* Done — and the file did
   not exist, so it is authored here rather than inherited; I treated the
   spec's ten described assertions as the requirement. It is **eleven** tests,
   not ten: the extra one is item 5 below. All pass.
3. *`check-publish` verifies CI against the wrong commit.* Stated in the
   target's comment rather than fixed, because the fix you offered as the
   alternative does not exist: check runs for an *unpushed* commit are
   unconditionally absent, so re-reading after the changelog commit would
   refuse every release. The comment names the limit and what bounds it.
4. *The duplicate dirty-tree refusal.* Kept, with a comment saying it must
   refuse before the changelog commit and that neither copy is redundant.
5. *`PUBLISH_VERSION` expands on every make invocation.* Confirmed:
   `--next-version` is one `git rev-list` and touches neither the network nor
   `gh`. It prints nothing and exits 0 when it cannot answer, and
   `check-publish` refuses on the empty result;
   `next_version_degrades_to_nothing_outside_a_repository` is the eleventh test
   and pins exactly that.

**Spec 2 (#986)**

1. *The SVG contradicts its own doc comment.* Kept the gradient and amended the
   comment, which is one of the two arms you offered. It now says the gradient
   is the one thing `MIC_SVG` does not exercise, that resvg implements SVG 1.1
   gradients in full and a two-stop vertical one is the least exotic there is,
   and that a flat field would make the About window differ from the Dock in
   the one constant a reader would never guess at. The comment no longer
   asserts a rule the artifact beside it breaks. Output bytes are unchanged, so
   both stated hashes still hold.
2. *Run `make app-test`.* Run, and it did **not** complete — reported honestly
   rather than claimed. Every crate compiled, including `tasks-gpui` itself,
   and the run died at the final link: `collect2: fatal error: ld terminated
   with signal 9 [Killed]`, the OOM killer, linking ~260 objects of the gpui
   stack in a 7 GB VM. So `about.rs` **type-checks** (the link is strictly
   after that) but its unit test was not executed here. `make app-test` on a
   machine with more memory is the one gap in this change, and it is the same
   gap the spec already named for the visual review.
3. *"Exactly two `<path`s" contradicts the redesign rule.* Dropped the count.
   The `about.rs` test asserts an `<svg ` root, a `</svg>` close and at least
   one `<path `, and says in its own doc comment that it is structure only —
   the constants stay the whole design surface.
4. *Soften the hash-mismatch instruction.* Moot in the event: I ran the
   committed generator, both hashes matched exactly, and `--check` round-trips
   clean. The discriminator you gave is what I would have used.
5. *The About window inherits the Dock margin.* Took the second arm: the
   generator emits a second mark, `app-gpui/icon/AppIconMark.svg`, from the
   same constants with the 100px margin cropped out of the viewBox — same two
   paths, same colours, one fewer thing to compensate for by eye. `about.rs`
   embeds that one. `AppIcon.svg` is unchanged and still committed at its
   stated hash. This is the one deliberate deviation from the spec's listing of
   `appicon.py`; the `svg()` function grew a `tight=False` parameter, and the
   default path emits the identical bytes.

## Directions

- *Base is another build's branch; #1070 changed the Makefile's images gating —
  read the current recipe.* Done; both Makefile edits were made against the
  tree in front of me, and they land in different places (the `app-install`
  recipe, and a new block after `release-clean`). No conflict with #1070's
  `images-rebuild` / `tasks hold` work.
- *Keep them as two separable commits.* Done — `366f6a1` (#997) and `6ea6cc4`
  (#986). Neither touches the other's files except the Makefile, in
  non-overlapping regions.
- *Run the committed generator rather than transcribing its output.* Done, and
  both hashes reproduced exactly. Nothing to investigate.
- *Run `make app-test` yourself and say so.* Done, and it is the one thing that
  did not finish — see review item 2 above for exactly how far it got.
- *Run `cargo test -p tasks --test changelog` first.* Done; the file had to be
  written first, and the eleven tests pass.

## What was run

`cargo test -p tasks --test changelog` (11 passed), `cargo test -p tasks --test
app_icon` (4 passed), `cargo fmt --all -- --check` (clean),
`cargo clippy --workspace --all-targets` (no errors), `python3
app-gpui/icon/appicon.py --check` (all three artifacts up to date),
`make -n` on all seven new targets and on `app-install` (the `cp` expands), and
a dry two-release CHANGELOG assembly confirming the prose stays above the
newest section. `make app-test` is the exception, above.
