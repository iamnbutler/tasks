# site/

The landing page for [nate.rip/tasks](https://nate.rip/tasks/) (#995). One
hand-written HTML file, one stylesheet, three screenshots. No build step, no
JavaScript, no webfonts, no analytics.

Preview it by opening `index.html` in a browser — there is nothing to run.
`make site-check` (or `bash site/check.sh`) is the gate: it compares the
disclaimer against the README's and resolves every relative `src` and `href`.

## Why there is no build step

Partly register — this is a page about a program, and a generator is a
dependency to keep alive for the rest of the page's life. Mostly a trap:
`.gitignore`'s `dist/` and `node_modules/` patterns are **unanchored**, so
they already match inside `site/`. A generator whose output landed in
`site/dist/` would commit nothing and fail silently.

## The two human acts, in this order

Both of these are repository settings and shell work on a Mac, so nothing
that lands on `main` can do either, and the pipeline cannot do them for you.
**Order matters.**

### 1. Archive what is serving now — before anything else

`nate.rip/tasks/` has been live since March, serving an mdBook titled "Tasks
Specification" with the book at `/tasks/latest/` and a `/tasks/versions.json`
beside it. It is a v1-era document, superseded by #744 and `docs/plans/`, and
replacing it is the point of this page.

But **there is no `gh-pages` branch on origin** — 84 heads, none of them —
and nothing in this repository reproduces that book. Pages simply kept
serving the last deployment after its source branch was deleted. The live
site *is* the artifact, it is the only copy of itself, and flipping the
source in step 2 destroys it with no undo.

So mirror it first, and put the copy somewhere durable:

```sh
wget -mkEpnp https://nate.rip/tasks/     # or equivalent
```

This cannot be automated from inside the pipeline and must not be attempted:
agent leases are `anthropic` plus repo-scoped git read (`Scopes::AGENT` in
`crates/tasks/src/broker.rs`), so a Builder cannot fetch `nate.rip` at all.

The book is deliberately **not** copied into `site/latest/`. Carrying a stale
spec forward at a live URL is worse than a dead link, and half-serving it is
worse than both. `/tasks/latest/` and `/tasks/versions.json` stop resolving
when step 2 happens; that is a deliberate loss, taken with the archive in
hand.

### 2. Flip the Pages source

Repo **Settings → Pages → Source → GitHub Actions**, once. Until that is
flipped, the March artifact keeps serving no matter what merges here.

This is also why gating the deploy on the screenshots costs nothing: the
publish is human-gated anyway, and both acts happen in one sitting.

## Deploying

`.github/workflows/pages.yml` does it, on a push to `main` that touches
`site/**` or `README.md`, and on `workflow_dispatch`. It runs
`bash site/check.sh`, then uploads `site/` and deploys it.

**The workflow has no `pull_request` trigger and needs none.** A Builder branch
is pushed into this repository, so a `push` trigger already attaches its check
runs to the commit a pull request points at — which is how
`.github/workflows/ci.yml` became evidence the orchestrator's merge brief cites
(#1015). What this one must never grow is `pull_request_target`, which would run
fork code with this repository's secrets;
`no_workflow_runs_fork_code_with_our_secrets` in `crates/tasks/tests/site.rs`
fails the suite if any workflow does. Note that this workflow is filtered to
`site/**`, so it does not run on most branches at all — an absent check is not
a passing one, and `Landing` reports only the checks that exist.

Pushing a file under `.github/workflows/` needs `workflow` scope on the
token doing the pushing (classic PAT: the `workflow` checkbox; fine-grained:
Workflows: write). If the server's token lacks it, GitHub rejects the push
with a message naming the scope, the build fails as `FailureClass::Egress`,
and its bundle is preserved at `<scratch_root>/rejected/<build_id>.bundle`.
That is a token problem, not a code problem — recover with the `git fetch`
the failure prints and fix the token. Do not "fix" it by deleting the
workflow file and shipping a page nothing publishes.

## No CNAME

`nate.rip` is the custom domain of the *user* Pages site, and GitHub serves
project sites underneath it. The mechanism is visible:
`curl -sI https://iamnbutler.github.io/tasks/` answers `301` to
`https://nate.rip/tasks/`. Adding a `CNAME` file here would be wrong.

The consequence is that **every path on the page must stay relative** —
`style.css`, `img/…`. A root-relative `/style.css` resolves to nate.rip's own
site, not to `/tasks/`.

## The disclaimer is a copy, and there is a contract

The risk block on the page is the README's `## Read this first` (#984),
verbatim. Both files carry it between `<!-- disclaimer:start -->` and
`<!-- disclaimer:end -->`, and two independent checks compare them after
normalising whitespace and markup:

- `disclaimer_on_the_page_matches_the_readme` in `crates/tasks/tests/site.rs`,
  which `make test` runs — **before a merge**
- `site/check.sh`, which the workflow runs — **before a deploy**

The duplication is deliberate. This pipeline merges its own pull requests, so
a drift only the deploy-time check catches has already landed on `main`, and
its first symptom is a page that stopped publishing. The README is canonical:
edit it, then make the page match.

`.nojekyll` is kept so nothing under `site/` is filtered by a Jekyll pass.
