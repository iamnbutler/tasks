# Signing, notarization and stapling the release artifacts

*2026-08-19, written against #988, whose decision — **Option B, signed and
notarized** — was taken and recorded on 2026-08-18 in
`docs/plans/2026-08-18-release-roadmap.md`. #991, #995 and #997 branch on it
and may assume a notarized artifact. This document is the second half the
issue asks for: the checklist, the pipeline, the pitfalls, and the answer to
the one question the issue left open — what happens to the `tasks` CLI that a
notarized app shells out to.*

## What this is for

`make app` and `make dist` are unsigned and stay unsigned. They never leave the
machine that built them, which is exactly why they are fine. A **download** is
not: a bundle that arrives with `com.apple.quarantine` on it and no Developer ID
signature is refused by Gatekeeper, and the refusal lands on someone who has no
terminal in front of them and no reason to trust a right-click-Open workaround.

So there is a second chain, `make release`, that turns a built bundle into a
signed, notarized, stapled `.app` + `.dmg` plus a signed, notarized standalone
CLI archive, all staged in `dist/`.

**Every target in it refuses rather than degrades.** No signing identity is an
error, never an unsigned artifact. The failure worth preventing here is the
quiet one: the `.dmg` that built fine, got uploaded, and reached a link
unsigned.

## The artifacts

| artifact | signed | notarized | stapled | verified by |
| --- | --- | --- | --- | --- |
| `dist/Tasks.app` | yes, hardened runtime, timestamped | yes | **yes** | `codesign --verify --strict`, `stapler validate`, `spctl -a -t exec` |
| `dist/Tasks.app/Contents/Helpers/tasks` (the seed) | yes, hardened runtime, timestamped, signed **first** | yes, as nested code of the app | covered by the app's ticket | `codesign --verify --strict` |
| `dist/tasks` → `dist/tasks-<version>-macos-arm64.zip` | yes — it is a **copy of the signed seed** | yes, in the same submission | **no — a bare Mach-O cannot be** | `codesign --verify --strict`; Gatekeeper fetches the ticket online |
| `dist/Tasks-<version>.dmg` | yes, timestamped, **no** `--options runtime` | yes | **yes** | `stapler validate`, `spctl -a -t open --context context:primary-signature` |

Names are `Tasks-<version>.dmg` and `tasks-<version>-macos-arm64.zip`, both off
`BUILD_VERSION` (`0.1.<commit count>`), because #997 tags releases
`v0.1.<commit count>` and the build stamp already carries that number
everywhere else. One number, not four.

`arm64` is in the CLI archive name because that is what this ships. There is no
universal binary and no Intel build; `lipo` plumbing for a slice nobody has
asked for is out of scope.

## `dist/` is not what `make dist` makes

Worth knowing before typing either. **`make dist` does not write to `dist/`** —
it installs to `$HOME/Applications/Tasks.app`, exactly as it always did.
`dist/` is this chain's staging directory (it was gitignored and unused, which
is why the name was free), and `make release-clean` empties **only** that. It
will not tidy up after a `make dist`.

## The pipeline

`make release` is the whole chain, and it runs the cheap refusals first so a
missing credential costs a message rather than a six-minute build:

```
check-signing       SIGN_IDENTITY set, and present in this keychain
check-notary        notarytool exists, and NOTARY_PROFILE answers
check-clean-tree    a release names a commit (FORCE=1 overrides)
release-bundle      app-build + server-release + app-install + dist-install,
                    with APP_BUNDLE=dist/Tasks.app on the sub-make command line
sign                seed first, then the bundle; copy the signed seed to dist/tasks
notarize            ONE submission carrying both; staple the app; write the CLI zip
dmg                 refuses unless the app is stapled; build and sign the image
notarize-dmg        submit, staple
verify-release      codesign + stapler + spctl on everything
```

Each is also a target on its own, because a notarization retry should not
rebuild and a re-sign should not resubmit.

Three orderings inside it are load-bearing:

- **`release-bundle` reuses `app-install` and `dist-install`** rather than
  copying them, by overriding `APP_BUNDLE` on the sub-make command line. A
  command-line variable beats the `:=` in the Makefile and propagates to
  sub-makes, so nothing else needed to change. A second pair of recipes aimed
  at `dist/` is how the release bundle and the dev bundle would come to differ
  in a way nobody noticed until a download behaved unlike the thing that was
  tested.
- **`dist-install` precedes `sign`.** Codesigning seals the bundle's resources,
  so anything added afterwards invalidates the signature.
- **`notarize` precedes `dmg`,** and `dmg` refuses unless
  `stapler validate` passes on the bundle. That precondition is the point: a
  DMG built before notarization looks identical and ships an app whose first
  launch needs Apple reachable.

`stapler` is the gate, not `notarytool --wait`. A ticket exists only for an
Accepted submission, so a staple that succeeds is the proof and a staple that
fails is where a rejection stops the chain.

`check-signing` and `check-notary` are split because they fail for different
reasons and only `sign` and `dmg` need an identity.

## The CLI question, answered

The issue flags this as "its own conversation". It has three parts.

1. **The seed inside the app is nested code.** `Contents/Helpers/tasks` is
   signed with the same identity and the same hardened runtime, notarized with
   the app, and covered by the app's stapled ticket. `tasks service install`
   copies it to `~/.tasks/bin/tasks`; a Mach-O signature lives *in the file*,
   so it survives the copy and launchd runs a validly signed binary.
2. **The standalone CLI archive cannot be stapled.** Apple staples bundles,
   disk images and flat installer packages only; against a raw executable
   `stapler` fails looking for `Contents/CodeResources`. So
   `tasks-<version>-macos-arm64.zip` is notarized-but-not-stapled, and
   Gatekeeper fetches its ticket **online** at first launch of a quarantined
   copy. `make notarize` prints this as output rather than leaving it here,
   because whoever runs the chain is the person who would otherwise try it.
3. **That is acceptable for a reason specific to this system, not in
   general.** Nothing in Tasks works offline: it polls GitHub every minute,
   leases Anthropic credentials through the broker, and clones repositories. A
   machine that cannot reach Apple to fetch a ticket cannot run the thing it
   just unzipped either.

   The stapleable alternative is a signed and notarized `.pkg`, which needs a
   **Developer ID Installer** certificate — a different certificate class from
   the Application one used here. Named and deferred, not overlooked.

## The checklist

The first block needs **no Apple account**. Run it tonight. The second block is
gated on enrollment and is days away; nothing in the first depends on it.

### Block A — before enrollment (no Apple Developer account needed)

1. `make -n release` and `make -n` on each of `check-signing`, `check-notary`,
   `release-bundle`, `sign`, `notarize`, `dmg`, `notarize-dmg`,
   `verify-release`, `check-clean-tree`, `release-clean`. Every recipe parses
   and expands to the paths you expect.
2. `make check-signing` with nothing configured. It must refuse, name
   `SIGN_IDENTITY`, and print `security find-identity -v -p codesigning` as the
   fix. Then `make check-notary`, which must refuse separately and print the
   `notarytool store-credentials` invocation.
3. `security find-identity -v -p codesigning` — answers whether this machine
   has any codesigning identity at all, which is the question enrollment is
   about to change.
4. `make dist` still installs to `$HOME/Applications/Tasks.app` and still
   works. `make release-bundle` is the only change to existing behaviour and it
   only overrides `APP_BUNDLE`.

### Block B — after enrollment (gated on the Apple Developer Program)

Prerequisites, all human-only:

- Apple Developer Program enrollment complete (**days** of lead time).
- A **Developer ID Application** certificate in the login keychain. Not an
  Apple Development one: the notary service rejects that by name.
- An app-specific password from appleid.apple.com, stored once:
  `xcrun notarytool store-credentials tasks-notary --apple-id <id> --team-id <TEAM> --password <app-specific-password>`.
  It goes in the keychain and nowhere else — never in the Makefile, in argv, in
  `.env` or in the repo.

Then:

5. `make release SIGN_IDENTITY="Developer ID Application: … (TEAMID)"`. Expect
   roughly a six-minute build plus notarization wait.
6. `make verify-release`. `spctl -a -t exec -vv dist/Tasks.app` must say
   `accepted / source=Notarized Developer ID`, and the DMG must pass
   `spctl -a -t open --context context:primary-signature -vv`.
7. **Download it on a clean machine** — a real download, so the quarantine
   attribute is really set — mount the DMG, drag to `/Applications`, launch.
   Nothing else in this list substitutes for this step.
8. **The quarantine gate.** After first launch installs the service, run:

   ```
   xattr -p com.apple.quarantine ~/.tasks/bin/tasks
   ```

   The expected answer is **`No such xattr`**. If it prints a quarantine
   string, the one-button install has produced a launchd-executed binary that
   triggers a Gatekeeper assessment on every exec, and **no download link may
   be published until that is fixed**. See the next section — this is
   confirmed behaviour, not a hypothesis, so treat a clean answer as the
   surprise and a dirty one as the expectation to check for.
9. `tasks secrets rehome-key`, once the signed binary is the one in
   `~/.tasks/bin` — see "The #1003 payoff" below.

**Expect the first notarization to be slow.** Stuck submissions are reported to
concentrate on newly enrolled accounts, which is exactly the state this account
will be in. A genuine signing defect comes back *fast* as Invalid; a submission
sitting In Progress for hours with clean local verification is the service, not
the signature. Do not read a queue delay as a defect and start re-signing
things — re-signing invalidates any staple and starts the clock again. Do not
schedule the first release tight against the enrollment.

## The quarantine propagation: confirmed, not suspected

`install_binary` in `crates/tasks/src/service.rs` copies the seed with
`std::fs::copy`. On macOS that reaches for `fclonefileat`/`fcopyfile`, which
**clone extended attributes**. This was measured on a Mac during review of this
spec: a file carrying `com.apple.quarantine`, copied through `std::fs::copy`,
arrives with `com.apple.quarantine` intact — and with `com.apple.provenance`
alongside it.

So the chain is not a hypothesis:

> downloaded, quarantined bundle → `tasks service install` →
> `~/.tasks/bin/tasks` carries the attribute → launchd's exec triggers a
> Gatekeeper assessment

That is the **one-button install**, on a fresh machine, which is the entire
path this distribution work exists to serve, and it is the path where a failure
is least recoverable because there is nobody at a terminal.

It is a numbered gate (step 8) rather than a pitfall for a reason: somebody
executing the checklist must treat it as expected and check it, not read past
it as a maybe.

**No change is made to `install_binary` in this work.** The fix belongs with a
real signed artifact to test against; shipping it blind is the same guess in
the other direction. When it is made, the ordering rule is fixed: clear the
attribute **after** verifying the copy's own signature, never before. Clearing
first would strip the one marker that says an unverified file came from
outside.

## Pitfalls

- **`--deep` is the trap.** It re-signs nested code with the *outer* bundle's
  arguments. The documented failure is a signature that verifies locally and
  comes back Invalid from the notary service minutes later. Sign inside-out
  instead: the seed, then the bundle.
- **A notary submission can hang rather than fail.** Covered above; it is why
  `sign` checks hardened runtime, timestamp and `get-task-allow` locally, where
  the answer takes a second.
- **Ordering is sealed.** Codesigning seals resources, and any later
  modification — *including re-signing* — invalidates a stapled ticket and
  means re-notarizing.
- **Rust binaries arrive ad-hoc signed** (the linker does it on Apple
  Silicon), so `--force` is required. Without it `codesign` fails on an
  already-signed binary rather than doing nothing.
- **App Translocation.** A quarantined app launched from where it was
  downloaded, or straight off a mounted DMG, runs from a randomized read-only
  path. Reading the seed still works, but the `/Applications` symlink in the
  DMG exists to make the drag the obvious gesture, and dragging out of the
  image is what clears translocation.
- **Certificates expire; timestamped signatures do not.** `--timestamp` needs
  Apple reachable at *signing* time, not only at submission time.
- **`app-gpui` is not a workspace member** and has its own `target/`, so the
  app binary and the server binary come from two different build directories.
  `release-bundle` drives both.
- **No entitlements file is added, deliberately.** Hardened runtime needs no
  exception for a Metal app that spawns child processes and speaks HTTP, and an
  entitlements plist that grants nothing is a file that accretes grants. Add
  one only when a real hardened-runtime failure names the exception it needs —
  and never `--options` without `runtime` to make a crash go away.
- **`Contents/Resources/third-party/` is not code.** It is sealed as a
  resource, needs no signature of its own, and must keep travelling with the
  bundle (Apache-2.0 §4(a)).

## How this is verified — three tiers

Being precise about this matters, because "nothing can be checked" and "the
interesting half cannot be checked" lead to different decisions about when to
merge.

1. **On the Builder's Linux host, now.** `codesign`, `xcrun`, `security`,
   `hdiutil`, `ditto` and `spctl` are all macOS-only, so *nothing here can be
   executed*. What can: `make -n` on every new target, each generated recipe
   piped through `sh -n`, and — for the one behavioural change to an existing
   recipe — `make -o check-darwin app-install APP_BUNDLE=/tmp/x/Tasks.app`
   against fake executables, asserting the bundle lands at the override and
   `~/Applications` is untouched.
2. **On a Mac, before enrollment.** Block A above: `make -n` through every
   target, `check-signing` and `check-notary` refusing cleanly with their fixes
   printed, and `security find-identity -v -p codesigning`. This is more than
   nothing and it is worth running before the account exists.
3. **On a Mac, after enrollment.** Block B: `make release`,
   `make verify-release`, and the download-on-a-clean-machine step. Only this
   tier tests Apple's verdict, which is the correctness that actually matters.

**No automated test is added, and that is deliberate rather than an omission to
fix.** A Rust test asserting strings in the Makefile would pass while every
Apple invocation in it was wrong. The real verification is tier 3.

## The #1003 payoff

CLAUDE.md says of the `keyring` migration that it "buys nothing yet… the real
benefit arrives with a signed application identity (#988, undecided)". #988 is
decided, and this is that identity.

A macOS keychain access list is granted to an *application*, and an unsigned
dev build is a different application on every `cargo build` — which is why a
natively-stored unseal key re-prompts. A stable Developer ID signature is what
makes the access list survive. So `tasks secrets rehome-key` is worth running
**after** the first signed release is the binary in `~/.tasks/bin`, not before.

This does **not** demote `TASKS_SECRETS_KEY_FILE`. Developers build unsigned
binaries constantly, and that path stays first-class.

## Deliberately not here

- **No `.github/` workflow.** #1015 owns the first one, and it moves the
  `land_builds` carve-out (c), `Landing::Clear::describe()`, its pinning test
  and the CLAUDE.md bullet in the same change as the workflow file. The targets
  here are written so a workflow calls them unchanged; CI additionally needs
  the certificate in a temporary keychain and an App Store Connect API key
  (`--key` / `--key-id` / `--issuer`) instead of a keychain profile.
- **No tag, no `gh release create`, no asset upload.** #997 owns the release
  flow and consumes these artifacts. If #997 lands first with its own unsigned
  `make release`, the composite target here is the sub-makes to fold into it;
  the individual targets are the deliverable either way.
- **No README change.** #994 owns the download link, and there is nothing to
  link to until a release exists. A 404 is worse than the build-from-source
  instructions already there.
- **No change to `install_binary`.** See the quarantine section.
- **No `.pkg`.** Needs a Developer ID Installer certificate; deferred, named.
- **No universal binary.** See `arm64`, above.
