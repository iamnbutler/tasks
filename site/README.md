# site/

The landing page for [nate.rip/tasks](https://nate.rip/tasks) (#995). One
static HTML file, no build step, no JavaScript, no external requests — the
CSS is embedded and the fonts are system stacks.

Preview it by opening `index.html` in a browser. There is nothing to run.

## Publishing

GitHub Pages serves a branch root or a `/docs` folder, never a `site/`
subdirectory — and `docs/` here is design docs, so the site goes out as a
`gh-pages` branch cut from this directory:

```sh
git subtree split --prefix site -b gh-pages
git push -f origin gh-pages
git branch -D gh-pages
```

Then (once): repo **Settings → Pages → Deploy from a branch →** `gh-pages` `/ (root)`.

The page lands at `nate.rip/tasks` because the user site
(`iamnbutler.github.io`) carries the `nate.rip` custom domain and project
pages inherit it; no CNAME file belongs here.

There is deliberately no Actions workflow for this. Adding
`.github/workflows` changes what `land_builds` may conclude about a PR (the
landing rule's carve-out (c)), and that move is #1015's — the workflow file
and the rule change go in the same commit, and a Pages deploy must not be the
change that smuggles it in.

`.nojekyll` rides along in the split so Pages serves the files as-is.
