# site/img/

Three screenshots, and what each one has to show. They are referenced by
`site/index.html` and `site/check.sh` errors until they exist — deliberately,
because a landing page with broken-image icons is worse than one with no
screenshots, and a held deploy leaves the previous page serving.

**These cannot be produced by an agent.** `app-gpui` compiles and its unit
tests run on Linux, but CLAUDE.md's boundary is explicit — compile and test,
yes; run, no. These are taken by hand on a Mac with a window server.

## The shots

| file | what it must show |
| --- | --- |
| `workspace.png` | The three-pane workspace: the task rail, a task selected with its overview tab, and the orchestrator chat. The shot the page leads with, so it is the one that has to read at a glance. |
| `agent-feed.png` | A Scout mid-run, on the live agent feed tab — tool calls and output streaming in. It has to be a *running* agent; a finished one is a different, duller picture. |
| `spec-review.png` | A spec in the review queue with its approve / request-revision controls visible. This is the decision the pipeline is built around. |

## Rules

- **Real content.** Real issue titles, real specs, real state. A failed scout
  in the queue is fine and arguably better — the page says this is alpha, and
  a screenshot with nothing wrong in it contradicts the copy two sections up.
- **Real density.** Captured at 2× and left at 2×; do not downscale.
- **Exactly 2560 × 1600**, all three, which is a 1280 × 800 window at 2×. The
  `<img>` tags declare those dimensions so the page does not reflow as the
  images load, and the same size for all three keeps the column from jumping
  between figures. A different size means editing `index.html` to match.
- **The whole window**, including the title bar. No device frames, no drop
  shadows, no rounded-corner compositing, no coloured or gradient background
  plates. The point is what the program looks like, not what a marketing
  render of it would look like.
- **Nothing private in frame** — repository names you would not publish,
  tokens, paths with your name in them, other people's issue text.
- **PNG, run through `oxipng`** (`oxipng -o max --strip safe img/*.png`).
  Lossless, and it strips the capture metadata that would otherwise carry the
  machine's name.

## If a shot cannot be taken

Delete its `<figure>` from `site/index.html` rather than committing a
placeholder. The page reads fine with two, and a placeholder screenshot is
precisely what #995 asked this page not to be.
