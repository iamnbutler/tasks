# A landing page for nate.rip/tasks (#995)

> **Read this first: my base already contained a different implementation of
> #995, and this change replaces it.** Commit `71e1cb7` ("The landing page for
> nate.rip/tasks (#995)") is on `main` ahead of me: a 166-line single-file page
> with embedded CSS, a `site/README.md` prescribing a `git subtree` push to a
> `gh-pages` branch, and an explicit refusal — *"There is deliberately no
> Actions workflow for this... that move is #1015's"*. The approved spec and
> its review require the opposite (a Pages workflow, gated by `site/check.sh`),
> and the reviewer verified independently that a **push-triggered** workflow
> produces no pull-request check, so `mergeable_state` stays `clean`/`dirty`
> and `Landing::Clear` keeps meaning what it says — which is the objection that
> sentence was raising. I have gone with the approved spec, replaced the page
> and rewritten `site/README.md`, and made the premise mechanically enforced
> rather than merely re-asserted (below). The base page also shipped three
> placeholder `<a>Download</a>` links with no `href`, which the spec's register
> rules forbid. **If the reviewer wants `71e1cb7`'s page kept, this is the
> commit to reject — it is not a merge conflict, it is a decision.**

Adds `site/` — one hand-written `index.html`, one stylesheet, a screenshot
manifest and a publish gate — and publishes it to `nate.rip/tasks/` from `main`
via `.github/workflows/pages.yml`. The page says what the program *is* in its
first sentence, carries the risk disclaimer as the second thing on the page,
shows the README's architecture diagram byte-for-byte, states the prerequisites
(including the ~22 GB RAM arithmetic at the shipped `SCOUT_MAX_CONCURRENT = 2`),
is plain that there is no release yet and names no version that does not exist,
and links to the repo, CLAUDE.md, the issues and `docs/plans/`. No build step —
partly register, mostly because `.gitignore`'s `dist/` and `node_modules/`
patterns are unanchored and would silently swallow a generator's output in
`site/dist/`. All paths stay relative: `nate.rip` is the *user* site's custom
domain and project pages are served underneath it, so a root-relative
`/style.css` would resolve to the wrong site and no `CNAME` belongs here.

The part that is more than a web page is `crates/tasks/tests/site.rs`. This
repository's first workflow file falsifies the claim that gates every
autonomous merge here — *"no `.github/workflows` and no branch protection"*,
whose truth was one `ls` away. Repairing that to *"no workflow produces a
pull-request check"* would trade a checkable fact for an unenforced one in the
one place this codebase refuses to, so the new claim is enforced instead:
`no_workflow_produces_a_pull_request_check` parses the `on:` block of every
workflow (ignoring `#` comments, so `pages.yml`'s own explanation of why it has
no such trigger does not read as one) and fails the suite naming all seven doc
sites that would become false. `disclaimer_on_the_page_matches_the_readme` sits
beside it: the page's risk block is the README's `## Read this first` (#984)
verbatim, both delimited by `<!-- disclaimer:start/end -->` markers, compared
with markup stripped and whitespace collapsed so a reflow is free and only a
changed *word* fails. `site/check.sh` makes the same comparison at deploy time;
the duplication is deliberate and commented in both files — this pipeline
merges its own pull requests, so a drift only the deploy-time check catches has
already landed.

**The four Rust files in this diff (`github.rs`, `brief.rs`, `builder.rs`,
`orchestrator.rs`) are comment-only** — no code, no signatures, no test
changes; `git diff` on them touches nothing but `///` lines, which I verified
mechanically rather than by eye. `Landing::Clear::describe()` and
`clear_says_what_it_does_not_mean` are untouched: `describe()` says "no check
here is capable of objecting", which stays true of a push-only workflow. I
checked that before editing, as the spec asked.

`make site-check` **fails, and that is the expected state of a correct
implementation.** It reports exactly three "screenshot missing" errors and
nothing else. The screenshots cannot be produced by an agent (`app-gpui`
compiles and tests on Linux but does not run there), the check errors rather
than warns because a held deploy leaves the previous page serving, and the
`<figure>` blocks must not be deleted to reach green. `site/img/MANIFEST.md`
says what each shot must show.

Two acts remain human and are listed in order in `site/README.md`: **mirror the
live deployment first** — `nate.rip/tasks/` has served a v1-era mdBook since
March, there is no `gh-pages` branch among the 84 heads and nothing in this
repo reproduces it, so the live site is the only copy of itself and the flip
has no undo — and only then flip Settings → Pages → Source to GitHub Actions.

## Review feedback

1. **Do not author a risk block in `README.md`; add markers around whatever
   #984 landed.** Done. #984 *has* landed (`e4d11a9`), so `README.md` gets
   exactly two added lines — the `<!-- disclaimer:start -->` / `<!-- disclaimer:end -->`
   markers around its `## Read this first`, with nothing changed between them.
   The page's copy is derived from it, so the spec's own three-paragraph draft
   is unused and the two texts have not diverged. `crates/tasks/tests/disclaimer.rs`
   still passes (its `prose()` strips HTML comments, so the markers are
   invisible to it). Re-run against the real base as instructed: **968 tests,
   not the spec's 894.**
2. **Add a workspace test asserting no workflow carries a `pull_request` /
   `pull_request_target` trigger, with a message naming the doc sites.** Done —
   `no_workflow_produces_a_pull_request_check` in `crates/tasks/tests/site.rs`,
   which names all seven sites and says what the assertion protects. I
   falsified it: adding `pull_request:` to `pages.yml` fails it, and a unit
   test pins that a *comment* mentioning the trigger does not.
3. **Move the disclaimer-equality check into that workspace test, keep
   `site/check.sh` and its three-way failure messages.** Done, both, with the
   deliberate duplication explained in a comment in each file so neither is
   deleted as redundant. Falsified all four of `check.sh`'s modes (drift, no
   block on page, no block in README, broken link) and confirmed a *reflow* of
   either file still passes.
4. **`site/README.md`'s human-acts list gains a first entry — mirror the live
   deployment before the flip, with the reason, and say order matters.** Done;
   it is step 1 of 2 under "The two human acts, in this order", with the
   `wget -mkEpnp` line, the no-`gh-pages`/no-undo reason, and a note that a
   Builder structurally cannot do it (`Scopes::AGENT` is `anthropic` +
   `git-read`, repo-bound). Your ruling on the substance — proceed with the
   replacement, do **not** copy the book into `site/latest/` — is recorded
   there as the decision rather than re-argued.
5. **The six doc repairs must be comment-only, and `SUMMARY.md` must say so.**
   Done and said, in its own paragraph above. The six are: `github.rs`
   (`Landing` doc, which now also names `pages.yml` and why it produces no PR
   check), `github.rs` (`clear_says_what_it_does_not_mean`'s doc), `brief.rs`
   (`verification_line`), `builder.rs`
   (`the_prompt_asks_for_the_verification_line_and_for_the_truth`),
   `orchestrator.rs` (`landing_section`), and CLAUDE.md's landing bullet.
   CLAUDE.md additionally gets the two *additive* entries the spec asked for (a
   `site/` entry in *Project structure*, a `make site-check` line in *Running*)
   — markdown, not one of the four Rust files.

## Directions

- **Read before you write; five other builds touch these files.** Done, and it
  changed the shape of the work — see the note at the top. The spec's line
  numbers had also moved (`github.rs` ~1856 is now ~2172), so every site was
  located by content and re-read before editing to confirm the sentence was
  still false in the expected way.
- **`README.md`: markers only.** Done — a two-line diff. #984's section was
  present, so the "ship your own words and put it at the top of `SUMMARY.md`"
  branch did not apply; what is at the top instead is the `71e1cb7` collision,
  which is the same kind of fact.
- **Four Rust files comment-only; stop if a signature, match arm or test
  changes.** Complied; nothing of the sort was needed.
- **The new workspace test: two assertions, failure messages written for
  whoever trips them in a year, and a comment saying the duplication with
  `check.sh` is deliberate.** Done, all three.
- **Do not fetch `nate.rip` or any other host.** Complied — no network request
  was made. Every fact about the live deployment in `site/README.md` is carried
  over from the spec's own reconnaissance and attributed as such.
- **Run `make test`, not a typecheck; report the real number. Run
  `site/check.sh` too and say which disclaimer failure mode it reported.**
  Done: `make test` is **968 run, 968 passed** (6 slow, 7 leaky — the expected
  scout-timeout leaks), doctests ok, exit 0. `site/check.sh` reported **none of
  its three disclaimer failure modes** — the disclaimer check passes; its only
  output is the three missing-screenshot errors described above.

Verification: PASSED — `make test` (968 tests run, 968 passed, 6 slow, 7 leaky; doctests ok; exit 0), plus `cargo fmt --check` and `cargo clippy --workspace --all-targets` clean with zero warnings. Separately, `make site-check` exits non-zero with exactly three "screenshot missing" errors, which is the intended state until the screenshots are taken by hand on a Mac.
