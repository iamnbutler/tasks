# The release flow: a tag, a changelog, and two downloadables

*2026-08-20, written against #997, consuming the artifacts of
docs/plans/2026-08-19-signing-and-notarization.md (#988, merged). That plan
deliberately ended at "no tag, no `gh release create`, no asset upload — #997
owns the release flow and consumes these artifacts." This is that flow.*

***Status: implemented** (#997). `scripts/changelog.sh`, the seven Makefile
targets, the bootstrap `CHANGELOG.md`, the `CLI_ZIP` rename and the inert-version
comment all landed; nothing has been tagged, because `make publish` delegates to
`make release` and Apple enrollment (Block B of the signing plan) has not
completed. Three `[as built]` notes below say where the implementation differs
from what was designed here.*

## What a release is

A release is a human choosing a commit on `main` and publishing two
downloadables built from it, under one version number, with a changelog entry
saying what changed since the last one. Everything else below is in service of
three properties:

- **One number.** The tag, the build stamp, the DMG name, the CLI archive
  name, the changelog heading and `GET /version` all say `0.1.<commit count>`.
  The stamp already carries that number everywhere (`build-stamp`, one
  implementation on purpose); the release borrows it rather than minting a
  second scheme.
- **Nothing leaves the machine until the artifacts are proven.** The order is
  build → sign → notarize → staple → verify, and only then tag → push →
  upload. A failed notarization retries without un-tagging anything, because
  no tag exists yet.
- **A release is a human act.** Cutting one decides what the project
  publishes, which is the `build-now` / `POST /projects` category — never
  charter-gated, refused to the orchestrator outright. Structurally enforced
  for now by there being no API route at all: the flow is `make publish` on a
  Mac holding the signing identity.

## The two artifacts

| conceptual name | file | what it is |
| --- | --- | --- |
| `tasks-client-macos` | `Tasks-<version>.dmg` | the app, signed, notarized, stapled — the thing a person double-clicks |
| `tasks-server` | `tasks-server-<version>-macos-arm64.zip` | the standalone `tasks` binary — the daemon/CLI for a headless Mac, signed and notarized (a bare Mach-O cannot be stapled; Gatekeeper fetches its ticket online, which the signing plan argues is acceptable *for this system*) |

Two naming decisions, one each way:

- **The DMG keeps `Tasks-<version>.dmg`.** That name is what Finder shows a
  person who just downloaded it, and `Tasks` is the app's name. The release
  asset's *label* carries the conceptual name ("Tasks for macOS (client)").
- **The CLI zip is renamed** from `tasks-<version>-macos-arm64.zip` to
  `tasks-server-<version>-macos-arm64.zip` — one Makefile variable
  (`CLI_ZIP`), free to change now and expensive after the first release
  exists. `tasks-<v>-macos-arm64.zip` is ambiguous about what it contains
  (the whole system? the client?); the binary inside is the server, and the
  name should say so. Note the same binary already ships *inside* the app as
  the seed at `Contents/Helpers/tasks` — the standalone zip exists for the
  Mac nobody sits in front of.

## The version, and why tags are now load-bearing

`BUILD_VERSION := 0.1.$(shell git rev-list --count HEAD)` is the identity, and
with #988 decided as Option B, #997's open question is answered: **a tag is
what a release is built from and what a bug report names**, not a bookmark.
Rules that keep that true:

- The tag is `v0.1.<count>`, **annotated**, created only by the release act,
  never moved, never deleted. Same append-only discipline as `decisions`.
- **A release is cut from `main` and nowhere else.** `git rev-list --count`
  counts all reachable commits, so it is monotone along one branch and
  meaningless across two: a release cut from a side branch can collide with a
  later `main` count. `check-publish` refuses any HEAD that is not
  `origin/main`.
- `[workspace.package] version = "0.1.0-alpha.1"` is declared **inert** with a
  comment pointing at `build-stamp` — the same class of fix as the
  `license = "MIT"`-with-no-LICENSE item. Moving it per release would buy
  nothing (nothing in this workspace publishes to crates.io; vm-pool's
  separate-publishability story is its own and untouched) and would add a
  second number to keep in step.
- `min_client_version` in `/version` is untouched by releases. It moves by
  hand on a wire break, exactly as today.

## The changelog

The raw material is already written. This repository's commit subjects are
sentences — "A broker outage holds dispatch instead of destroying the queue
(#1006)" — so the changelog job is **selection and assembly, not authorship**.

- **Generation rule** *[as built: not `--first-parent`]*: the walk is the full
  reachable set, keeping a commit when it is either on the first-parent chain or
  is itself a pull-request merge by subject shape. `--first-parent` alone is
  wrong on this repository's own history — `28c879e` ("Merge pull request #758
  from iamnbutler/feat/mac-app") is reachable from `main` and off its
  first-parent chain, so the bootstrap section would have shipped with the Mac
  app missing from it. It recurs whenever a build is merged into another build's
  branch rather than into the trunk, which this pipeline does routinely (the
  same stacking `POST /pull-requests/{n}/retarget` exists for), and it is what
  puts the denylist to work: under `--first-parent` those entries matched
  nothing. What was designed was
  `git log --first-parent <prev-tag>..HEAD --format=%s` —
  one line per landing on `main`, whether it landed as a merge or a direct
  commit. Subjects of the form `Merge pull request #N from …` are replaced by
  that PR's title (one `gh` call each); pure housekeeping lines are dropped by
  a small stated denylist (`Merge origin/main into …`, `Sweep: work the agent
  left uncommitted`, `Merge remote-tracking branch …`). The script is
  `scripts/changelog.sh <from> <to>`, deterministic, and *[as built]* tested
  against a **synthetic** repository rather than a fixture range: a Scout VM
  clones `--depth 50`, so a test pinned to real history passes in exactly the
  two places nobody is watching. The `gh` call is also *[as built]* the
  **fallback** rather than the rule — GitHub already puts the PR title in the
  merge commit's body, which is free, offline and un-rate-limitable.
- **`CHANGELOG.md`**, newest first. Each release is one section:

  ```
  ## v0.1.<n> — 2026-08-20

  One or two hand-written sentences, if the human offers them at publish
  time (`make publish HEADLINE="…"`). Optional; the bullets stand alone.

  - <commit subjects, verbatim>
  …

  [full diff](https://github.com/iamnbutler/tasks/compare/v0.1.<prev>...v0.1.<n>)
  ```

  No Added/Fixed/Changed categories: the subjects do not carry a category and
  imposing one means hand-sorting at publish time, which is the step that gets
  skipped and then rots the format. A flat list of sentences is honest and
  zero-maintenance.
- **The same section becomes the GitHub Release notes** (`--notes-file`). One
  generator, two destinations; if they ever differ, one of them is wrong.
- **The ordering paradox, named and resolved.** The version is the commit
  count, and committing the changelog moves the count — so the changelog
  commit is *inside* its own release, and the version is `count + 1`, computed
  before the commit so the section heading and the commit message
  (`Release v0.1.<n>`) can carry it. A single non-merge commit adds exactly
  one to `rev-list --count`, so the speculation is exact. The tag lands on the
  changelog commit. Computing the version *before* the changelog commit is the
  one-off-by-one mistake this paragraph exists to prevent.
- **git log remains the record between releases.** The changelog is a
  release-time digest, not a per-merge obligation — nothing about the
  pipeline's cadence changes.

## The release act: `make publish`

Refusals first, cheapest first, and nothing public until the artifacts are
verified — the same shape as `make release`, extended:

```
check-publish     HEAD == origin/main, clean tree, gh authenticated,
                  tag v0.1.<count+1> absent, and CI GREEN on HEAD —
                  read from GitHub's check runs at decision time, never
                  cached (a release must not race the suite; "still
                  running" refuses the same as red)
changelog         scripts/changelog.sh writes the new section; commit
                  "Release v0.1.<n>"
release           the #988 chain, unchanged: bundle, sign, notarize,
                  staple, dmg, verify-release
tag               annotated v0.1.<n> at HEAD; message is the headline,
                  or the section
push              git push origin main v0.1.<n> — atomically, both refs
                  in one command, so a rejected main push (someone merged
                  under us) leaves no orphaned tag
gh-release        gh release create v0.1.<n> --notes-file <section>
                  with both assets and their labels
verify-publish    fetch both asset URLs; non-200 or zero bytes fails
```

Each stage is its own target, because a notarization retry must not regenerate
the changelog and a failed upload must not re-notarize. If `push` is rejected
because `main` moved, the recovery is honest and cheap: reset the local
changelog commit, and start over against the new HEAD — nothing was published.

## Where it runs

**Phase 1 — the Mac, and this is the phase this document commits to.** Three
reasons, in descending weight:

1. The signing identity and the `tasks-notary` keychain profile live there,
   per the #988 checklist; enrollment is still in flight and Block B has not
   run yet.
2. When #1014 (publish the VM images) lands, pushing images becomes a step of
   the same publish act — and GitHub's hosted macOS runners do not offer
   nested virtualization, so apple/container cannot start a VM there. A CI
   release would still have a mandatory local half; a local release has one
   place where everything happens.
3. Two or three releases should prove the chain before it is automated —
   the first notarization on a fresh Apple account is documented (in the
   signing plan) to be slow in ways that look like failure.

**Phase 2 — `release.yml`, named and deferred.** `workflow_dispatch` only —
which keeps `no_workflow_produces_a_pull_request_check` true by construction,
and never `pull_request_target` for the reason ci.yml states. macOS runner,
Developer ID certificate imported into a temporary keychain from a
base64-encoded repository secret, and `notarytool` authenticating with an App
Store Connect API key (`--key`/`--key-id`/`--issuer`) instead of a keychain
profile — exactly the two substitutions the signing plan already names. The
make targets are the implementation either way; the workflow is a caller.
Revisit after the manual chain has produced two or three releases.

## What a release does not include, named

- **The VM images.** #1014's work. The coupling is stated now so it composes
  later: the registry tag must carry the same `0.1.<n>` as the release that
  expects those images (the signing plan already requires stamp == tag), and
  `make publish` grows an images step *when* #1014 lands — on the Mac, per
  phase 1 reason 2.
- **The download link.** #994 owns the README and site links, and the signing
  plan's step-8 quarantine gate governs them: **a release can exist — tag and
  assets up — while the link waits on the gate.** The first release is
  deliberately quiet until the clean-machine download test passes.
- **A cadence.** A release happens when a human wants one. No nightly, no
  release-per-merge: the pipeline merges its own PRs many times a day, and a
  tag that tracked merges would stop meaning "a human chose this".
- **Auto-update.** `tasks service install` is the upgrade path and
  `GET /version` already compares builds across processes; an update-check
  surface in the app is its own issue, not smuggled in here.

## Bootstrapping: the first release

The generator needs a previous tag and there is none. The first `CHANGELOG.md`
section is **hand-written** — an "initial release" paragraph plus highlights,
because a 1,000-commit bullet list is not a changelog, it is `git log` with
extra steps. The generator takes over at the second release, where
`<prev-tag>..HEAD` means something.

Order of operations, gated on Apple enrollment completing:

1. Signing plan Block B, through the clean-machine download test and the
   step-8 quarantine check.
2. Implementation below lands (it is independent of enrollment and can land
   first).
3. `make publish HEADLINE="…"` on the Mac.
4. The quarantine gate's verdict decides whether #994's link ships or waits
   on the `install_binary` fix.

## Implementation notes (what lands, sized S–M)

- `scripts/changelog.sh` + a test against a fixed historical range.
- Makefile: `check-publish`, `changelog`, `tag`, `push`, `gh-release`,
  `verify-publish`, composite `publish`; the `CLI_ZIP` rename. **The #988
  chain (`release-bundle` through `verify-release`) is consumed, not
  modified.**
- `CHANGELOG.md` bootstrap section (hand-written, at publish time). *[as
  built]* the file ships with its header now; the first `make publish` adds the
  first section.
- The `Cargo.toml` inert-version comment.
- CLAUDE.md: a short bullet under Running for `make publish`, and the
  human-only sentence.

Deliberately no automated end-to-end test of `publish` itself, for the reason
the signing plan gives about its own chain: the correctness that matters is
GitHub's and Apple's verdicts, and a test asserting the Makefile's strings
passes while every real invocation is wrong. The changelog script is the
testable part, and it gets the test.
