# Refuse browser-driven requests to the local API, and back the MIT claim with a LICENSE

Two independent specs on one branch, with no file overlap between them.

**#985 — the loopback guard.** The API's access control was its bind address,
and against a browser a bind address is not access control: a request with no
`X-Tasks-Actor` is read as the human's, and the human is never charter-gated,
so any page you had open could drive the pipeline two ways — a CORS-*simple*
`POST` (no body, no `Content-Type`, hence no preflight) whose opaque response
does not matter because `POST /tasks/{id}/build-now` has already dispatched a
VM that writes code and opens pull requests, and DNS rebinding, where a name
the attacker controls resolving to `127.0.0.1` makes their page genuinely
same-origin and lifts the simple-request restriction entirely. The new
`crate::loopback` is one `axum::middleware::from_fn` over the whole router
enforcing two rules that are not interchangeable, because each is blind to the
other's path: every authority the request states must name this machine's
loopback, and an `Origin` header — any value, `null` and a loopback one
included — is a refusal, on reads as much as on writes. Route registration
moved into a private `fn routes`, and the layer wraps *that*, so a route added
later is guarded by construction rather than by remembering; the property is
pinned on an **unrouted** path answering 403 rather than 404, which is what
makes it hold for routes nobody has written yet (stubbing the layer out was
run, and that is the test that goes red). No new dependency, no configuration
knob, and no `.rs` change outside `loopback.rs`, `lib.rs` and the router split.

**#983 — the LICENSE.** `Cargo.toml` has claimed `license = "MIT"` since the
workspace existed with nothing behind it, so the effective grant was
all-rights-reserved. This adds the MIT text at the root, a byte-identical copy
in the vendored `crates/vm-pool/` subtree, and one inside each of the three
vm-pool crates `cargo publish` can actually reach — not redundancy, since
`cargo package` ships only files under the *package* root and never walks up,
which was re-confirmed on this tree with a control (`vm-pool-supervisor`,
`publish = false`, no copy placed, no `LICENSE` in its file list). `app-gpui`
is `exclude`d from the workspace, so it could not inherit `license.workspace`
and asserted nothing at all; it now says MIT in its own manifest. The two
dependency premises the issue rests on are both wrong in the safe direction and
are corrected in the README: `gpui-unofficial` *is* Apache-2.0 and ships
`LICENSE-APACHE`, and `gpuikit` is `MIT OR Apache-2.0` rather than Apache-only,
so the redistribution blocker the issue asserts does not exist. What does exist
is the obligation to carry that Apache-2.0 text with the bundle, which is
handled below.

## Review feedback

### Spec 1 (#985)

- **"`GET` is covered as much as `POST`" is false — restate the coverage
  accurately.** Done, and the claim is gone from every place it would be read.
  `loopback.rs`'s module doc, the new CLAUDE.md rule and `docs/clients.md` all
  now say the same thing: `GET` is covered against *rebinding* by the authority
  rule and is **not** covered against a direct-to-loopback cross-site
  subresource load, because browsers send no `Origin` on `<img src>`,
  `<script src>` or `<iframe>`. The residual is stated as bounded to routes
  whose responses the attacker cannot read (no CORS headers) and whose only
  effect is server-side, and **`GET /decisions/{seq}/reconcile` is named as the
  one route where it is not nil**.
- **Say whether that route is accepted or moved to `POST`.** **Accepted**, and
  the reasoning is written down rather than implied: it is idempotent, mutates
  nothing locally, answers only for a decision still `pending` (verified —
  `server.rs` returns 400 for any other state before it touches GitHub), and
  the obligation loop and the orchestrator's own documented `curl` both name it
  as a `GET`. What it costs an attacker who can make you load a page is one
  GitHub read per pending decision: a rate-limit lever, not the `build-now`
  hole. Moving it would be a wire break for a benefit `Sec-Fetch-Site` gets
  properly, and the direction for this build was explicit that the guard must
  not be widened here.
- **"No change to any existing client" is falsified by the menubar app.**
  Corrected. The claim is now about *deployment shapes* rather than client
  libraries, in `docs/clients.md` and in CLAUDE.md: through an SSH `-L` tunnel
  the client connects to `localhost:PORT`, the `Host` arrives loopback, and
  nothing changes; through an HTTP reverse proxy (`tailscale serve`, nginx) the
  proxy forwards its own `Host` and every request 403s. That is stated as
  **accepted breakage with the tunnel as the answer**, naming
  `TASKS_MENUBAR_MACHINES` as what can express the broken shape.
- **Re-examine "no knob" now that its premise has lost a leg.** Done, and the
  conclusion is still no knob, but on a different argument. An
  `X-Forwarded-Host`-aware allowance is **rejected explicitly rather than
  unconsidered**: it would trust a header any client can send, on a listener
  with no way to tell a proxy from a web page — the guard deleting itself. A
  trusted-authority list is named as the honest shape and tied to the bind:
  this guard assumes `server::bind`'s `Ipv4Addr::LOCALHOST`, so if Tasks ever
  binds beyond loopback the allow-list widens *and* something real goes in
  front of the port, deliberately, rather than a switch being flipped.
- **State how the client claims were verified, and where you looked.** Done
  here rather than only in the spec, since this is the claim the "no knob"
  decision rests on. `tasks-client`: `grep -n '\.header(\|\.set(' crates/tasks-client/src/*.rs`
  is empty across both `lib.rs` and `sse.rs`, so ureq derives `Host` from the
  URL and sends no `Origin`. `tasks reload`: the three `reqwest::Client`
  builders (`reload.rs:1117`, `:1285`, `:1311`) set a timeout and nothing else,
  and every URL is built as `http://127.0.0.1:{port}/…` — no `.header()` call
  on any of them. The orchestrator: `curl_config_contents`
  (`orchestrator.rs:705`) emits one `header =` line, the actor header, and a
  unit test already pins that count. The menubar probe: `machines.rs:57` builds
  a `tasks_client::Client` with `Client::with_base`, so it is the same ureq
  path and the same finding — which is exactly why the break there is about the
  *authority in the URL*, not about a header the client adds. All four shapes
  are also pinned by tests rather than left as prose:
  `the_shapes_real_clients_send_are_untouched` drives `127.0.0.1:p`,
  `localhost:p` and `[::1]:p` through the real router over a real socket.

### Spec 2 (#983)

- **The bundle ships Apache-2.0 code and would have asserted only "MIT
  licensed".** Took the first option: the license text now travels with the
  binary. `app-gpui/third-party/` holds `LICENSE-APACHE-2.0` (the file
  `gpui-unofficial` 1.14.2 itself ships, verbatim including its
  `Copyright 2022 - 2025 Zed Industries, Inc.` header),
  `gpuikit-LICENSE-MIT`, and a `NOTICES.md` naming each component, its
  version/rev, and which arm of a dual license was taken. `make app-install`
  copies the directory into `Contents/Resources/third-party/` — in
  `app-install` rather than `dist-install`, because `make app` is a
  redistribution too the moment anyone hands the bundle over. The
  `NSHumanReadableCopyright` string is written to match: *"Tasks is MIT
  licensed; this bundle also contains Apache-2.0 components — see
  Contents/Resources/third-party/NOTICES.md"*, not a flat MIT claim.
  `gpui-platform-gpui-unofficial` ships a byte-identical Apache text (md5
  `776e07ed20b75b675553b3a113323c42`), so one file serves both, and that is
  said in `NOTICES.md` rather than left as a silent dedupe. Neither Apache
  component ships a `NOTICE` file, so §4(d) — conditional on one existing —
  imposes nothing further; what is discharged is §4(a).
  **One deliberate partial decline**: the About window is *not* wired to open
  the notices. The review left rendering to my call, and a GUI change I cannot
  run from this VM (`make app-test` proves logic, not that a pixel landed) is
  the wrong thing to ship untested when the actual obligation — the text
  travelling with the binary — is discharged by the Resources copy and pointed
  at from the plist and the README. It is a small, safe follow-up on a Mac.
  This also means two files beyond the spec's stated set are touched:
  `Makefile` (one `cp -R`, plus the comment explaining why) and the new
  `app-gpui/third-party/`.
- **Check the copyright year against when the project actually started, and say
  what you used.** Done, and the spec's own pitfall did not apply here: **this
  Builder clone is not shallow** (`git rev-parse --is-shallow-repository` →
  `false`), so I read it from the full history rather than from the GitHub API.
  `git log --reverse` gives `626c4e8 2026-03-11 "Initial spec draft for Tasks
  platform"` by `Nate Butler <iamnbutler@gmail.com>`, and
  `git log --format='%ad' --date=format:'%Y' | sort -u` returns exactly one
  line: `2026`. So a single `2026` is right and a range would claim authorship
  years that do not exist — the same conclusion the spec reached, now from a
  stronger source, and corroborated by the repo `created_at` the spec cites
  (2026-03-12) and by `docs/plans/2026-04-17-…`. The README says this in one
  sentence so the next reader does not re-derive it.
- **Say where the third-party license facts came from.** They came from files I
  downloaded and read in this VM, not from recollection, and the commands are
  worth repeating because the spec's own guess about provenance was wrong: the
  agent image has **no** gpui crates in `~/.cargo/registry` (`app-gpui` is
  excluded from the workspace, so they were never fetched), and `crates.io` API
  answers 403 from here. `static.crates.io` does answer, so
  `curl -sSL https://static.crates.io/crates/gpui-unofficial/gpui-unofficial-1.14.2.crate`
  → `tar` gives `Cargo.toml` with `license = "Apache-2.0"` and one
  `LICENSE-APACHE` at 10768 bytes, sha256
  `752daf2f…9d53da7a`; the same for
  `gpui-platform-gpui-unofficial-1.14.2`. `gpuikit` is a git dependency, and
  `raw.githubusercontent.com/iamnbutler/gpuikit/b28732f2…/Cargo.toml` answers
  200 with `license = "MIT OR Apache-2.0"`, alongside both `LICENSE-MIT` and
  `LICENSE-APACHE`. That `LICENSE-MIT` is also the canonical text the root
  `LICENSE` was diffed against — identical modulo the copyright line, with no
  `<year>`/`[fullname]` placeholder left in it, checked mechanically.

## Directions for this build

- **Two specs, one branch, no file overlap.** Done as described.
- **Account for every required change in the review feedback, declines
  included.** Above; the one partial decline (the About window) is stated with
  its reason.
- **`README.md`: `## License` at the end, touch nothing else — no reflow, no
  reordering, no formatter.** Done. `git diff --stat README.md` is **34
  insertions, 0 deletions**: the section is appended after
  `## Reading further`, and nothing above the last existing line is touched.
  #984's `## Read this first` at the top and #1003's credential edits in the
  middle are in different regions.
- **`CLAUDE.md`: add the rule after the attribution rule, leave the
  surrounding rules byte-identical.** Done. `git diff --stat CLAUDE.md` is
  **65 insertions, 0 deletions**. The new bullet is inserted between *"The
  charter only binds what the server can attribute…"* (which it is the other
  half of — that rule is why an unattributed request is the human's) and *"A
  refusal is a no-op…"*; both are unchanged byte-for-byte, as is everything
  else in the file.
- **`server.rs`/`lib.rs` are no longer contended — do not work around a
  conflict that is not there.** Confirmed: the base contains PR #1021, so
  `pool_health` is present in `lib.rs` and `Services`. The router split is a
  plain edit against the current shape with no accommodation for absent work.
- **Do not widen the guard to close the no-`Origin` subresource `GET` path;
  fix the claim, not the mechanism.** Followed exactly. `verify` implements two
  rules and nothing else — there is no `Sec-Fetch-*` read anywhere in the
  change — and the entire response to that finding is documentation, in the
  module doc, CLAUDE.md and `docs/clients.md`.

## Regions touched, for the merge order

- `README.md` — **appended only**, one new `## License` section (with a
  `### Third-party` subsection) after `## Reading further`. Byte 0 through the
  previous last line are unchanged.
- `CLAUDE.md` — **one new bullet inserted** in the load-bearing rules list,
  immediately after the bullet beginning *"The charter only binds what the
  server can attribute"* and immediately before *"A refusal is a no-op"*.
  Nothing else in the file is touched.
- Everything else is new files or additive edits: `LICENSE` ×5,
  `app-gpui/third-party/` (new), `crates/tasks/src/loopback.rs` (new), a
  module registration in `lib.rs`, the router split plus four tests in
  `server.rs`, a new subsection in `docs/clients.md`, two manifest lines plus a
  comment in `app-gpui/Cargo.toml`, one key in `app-gpui/Info.plist.in`, and
  one `cp -R` plus its comment in the `Makefile`.

## Tests

16 new tests. 12 unit tests in `loopback.rs` over `verify`/`is_own_authority`
(the spec called for 11) covering the authorities real clients send, a rebind
however it resolves, a port that is not a port, IPv6 bracketing and the
`::1:4800` ambiguity, userinfo, an HTTP/2 request with no `Host` header at all,
a smuggled second `Host`, an absolute-form authority disagreeing with the
header, and every origin including `null` and a loopback one. Four socket-level
tests in `server::tests` drive the real router over a real listener:
`the_guard_covers_a_path_with_no_route` (403 not 404, with a control asserting
the unrouted path is a plain 404 without the header),
`a_cross_origin_post_cannot_reach_a_handler` over `build-now`, `scout`, `queue`
and `runs/cancel-all`, `a_rebound_host_can_neither_read_nor_write` over
`/tasks`, `/decisions`, `/status`, `/version` and `POST
/pull-requests/1/merge`, and `the_shapes_real_clients_send_are_untouched`.
Every refusal test carries a control asserting the same call *without* the
header still reaches the handler, so each assertion is about the header and not
about a route that was broken anyway. Falsifiability was run rather than
claimed: deleting the `.layer(…)` turns
`the_guard_covers_a_path_with_no_route` red with `left: 404, right: 403`, and
restoring it byte-for-byte turns it green again.

Verification: PASSED — `make test` on the committed tree (910 tests run, 910 passed, 0 failed, 0 skipped; 6 slow and 7 leaky, all the documented expected ones; the `cargo test --doc --workspace` tail nextest does not cover also ran, 3 passed / 0 failed), plus `cargo clippy --workspace --all-targets` exit 0 with zero warnings, `cargo fmt --all --check` clean, and — since `app-gpui/Cargo.toml` changed — `make app-check` exit 0 and `make app-test` 245 passed / 0 failed.
