# Redact secrets at the point of formatting, in vm-pool and in the supervisors

`vm_pool_manager` logged the whole `container run` argument vector at INFO on every VM
allocation — a bare `?args` over a `Vec<String>` — and for an agent VM that vector carries the
agent API key as an ordinary `-e NAME=VALUE` pair. The key was written in plaintext once per
scout and once per build, into whatever sink vm-pool's tracing subscriber is attached to. The
fix redacts at the point of *formatting* rather than at the point of reading, and does it
through types rather than at a call site: a new `vm_pool_protocol::redact` scrubs **inside** the
formatter (`Scrubbed`, `ScrubbedDebug`), so a disabled log level costs nothing and no
unscrubbed `String` is left lying around; `ContainerArgs` is an owned newtype whose `Debug` is
the only rendering a `tracing` field can reach, with `as_str_refs()` as the deliberate way back
to the real arguments, going to `spawn` and never to a formatter; and `VmConfig` — which is
where the credential enters the system — loses its derived `Debug` for one that masks
secret-named `env` values, so the same leak is not one field away in a type every caller holds.
Three DEBUG lines that print a serialized command or event are wrapped too, and stay at DEBUG:
the level was never what made them safe. Redaction is name-based, never value-based, and it is
a property of formatting only — `Serialize` is untouched, so the wire format does not move and
the container still starts with the real key.

Grepping for siblings turned up the classic second instance on the other side of the wire: the
credentialed clone URL the server mints and hands to a VM, which both supervisors print in full
when they cannot decode a command line, into a stderr that is inherited up through
`container run` into vm-pool's own log. So `crates/tasks/src/redact.rs` moved down to
`crates/tasks-protocol/src/redact.rs` — the lowest place the server and both supervisors share
one implementation — with the old path kept as a re-export so the existing call sites are
untouched, and `Secret` was added there: a string whose `Debug`/`Display` are both `<redacted>`
and whose value is reachable only via `expose()`. `Config.github_token` is now one, which makes
`Config`'s derived `Debug` safe (a hazard, not an incident — nothing logs a `Config` today).
`scout-supervisor::git_clone` captures the clone's stderr instead of inheriting it and puts a
redacted tail in the error, and `builder-supervisor`'s `git`/`git_stdout` redact the arguments
they bail with, since the clone's arguments include the URL. The two redaction modules are
deliberately separate: `crates/vm-pool/*` must never depend on a tasks crate, and pointing
`tasks` at vm-pool instead would put a security control inside a vendored crate meant to stay
independently publishable. No test fixture, commit message, comment or line of this file
contains a real credential; every leak is named by its call site and its field.

## Merging this closes nothing

**Redaction stops *new* writes. It does not un-leak anything already written, and the exposure
is closed by rotating the credential — a human act that nothing in this pipeline can perform.**

The agent API key configured on this host has been written in plaintext, once per dispatch, for
as long as that INFO line has existed. It is in vm-pool's console scrollback, in any file its
stdout was redirected to, in any `launchd` `StandardOutPath`, and in every rotated, archived,
backed-up or pasted copy of those. A green test suite and a clean diff say the line will not
write it again; they say nothing about the copies that exist. Whoever owns that credential has
to rotate it.

The same applies to `GITHUB_TOKEN` if any of the clone-URL paths above were ever hit — it rides
inside every clone URL a VM is handed, so a supervisor that logged an undecodable command line,
or a `git` invocation that bailed with its arguments, put the token wherever that stderr went.

## What merging does not deploy

Two of the three halves need an operator action after the merge, and neither happens on its own:

- **The reported INFO line lives in `crates/vm-pool/pool`, which is what the long-lived
  `tasks vm-pool` daemon runs.** A server restart does not restart the pool, so the fix takes
  effect only once the **pool** is restarted — ahead of the server, per CLAUDE.md.
- **The three supervisor fixes land inside the Scout and Builder images and are inert until
  someone runs `make images`** on a Mac with apple/container and the cross toolchain.
  `make images-check` and the image freshness report are what say so in the meantime.

The tasks-side changes (`Secret`, the `redact` move) take effect with an ordinary server
restart.

## Review feedback

1. **State the deny-list's failure direction in the module's own docs, in a sentence a person
   adding a new credential will hit.** Done — `crates/vm-pool/protocol/src/redact.rs` opens with
   a *"This is a deny-list, and it fails open"* section saying a secret whose name matches none
   of the suffixes is logged in full, silently, and that adding a new credential to any
   environment this formats means adding its name shape there.
2. **Add a test pinning a deliberately unmatched name, so the boundary is a recorded decision.**
   Done — `an_unmatched_name_is_deliberately_not_redacted` asserts `ANTHROPIC_API_SESSION` is
   *not* redacted, and says in its doc comment that the fix is to extend the list rather than
   weaken the test.
3. **Close the near-misses: `_CREDENTIAL` singular, `_PAT`, `_AUTH`, `_PWD`.** Done — all four
   are in `SECRET_SUFFIXES` and covered by `secret_names_match_at_a_word_boundary`.
4. **Note the `AWS_ACCESS_KEY_ID` shape, where the suffix is present but not terminal.** Done —
   the module docs name that shape and explain why the family rule structurally cannot catch it,
   and a `SECRET_NAMES` list holds such names in full; `AWS_ACCESS_KEY_ID` and
   `AWS_SECRET_ACCESS_KEY` are its first entries, and the test covers them.
5. **`SUMMARY.md` must say in the PR body that merging closes nothing, and say the same about
   `GITHUB_TOKEN`.** Done — *Merging this closes nothing*, above, in its own section rather than
   as a closing note.
6. **Keep the no-real-credential property.** Kept. The fixtures are `not-a-real-credential-0000`
   and names like `github.example.com`; no real log line is quoted anywhere, and the bug is
   described by its call site (`crates/vm-pool/pool/src/lib.rs`, `?args`) rather than by its
   output.

Nothing in the feedback conflicted with the spec.

## Directions

1. **Never reproduce the key or any fragment, anywhere.** Held — in the code, the tests, both
   commit messages, this file and the comments. Every leak is named by call site and field.
2. **`SUMMARY.md` must say merging closes nothing, in its own section, including
   `GITHUB_TOKEN`.** Held — see above.
3. **Also state what merging does not deploy: the supervisor half needs `make images`, and the
   INFO line needs the pool restarted ahead of the server.** Held — *What merging does not
   deploy*, above.
4. **Document that `is_secret_name` fails open, pin it with a test, add `_CREDENTIAL`, `_PAT`,
   `_AUTH`, `_PWD`, and note the `AWS_ACCESS_KEY_ID` shape.** Held — items 1–4 of the review
   feedback above are the same requirement and are done once.
5. **`crates/tasks/tests/scout.rs:667` is the known flake #958 — name it and move on if red.**
   It was green in this run, so there was nothing to name; the assertion was not touched.
6. **Do not end the turn waiting on a background command.** Held — every command ran in the
   foreground and completed.

No direction conflicted with a spec.

## Notes on the implementation

The one place the spec's own numbers moved: it reported 12 unit tests on the new vm-pool module
and 789 tests passing overall; this branch has 13 (the extra is the required unmatched-name
test) and the suite is now 795. The scrubs' ordering is worth knowing about when reading the
module: `scrub_text` runs the URL pass **first**, because a
`…x-access-token:<token>@host` handled the other way round would be masked from the `token:`
onwards and take the host with it — and the host is the operational half of the line.
Idempotency is pinned across both scrubs, including against text already scrubbed by the
tasks-side redactor, which masks with `***@` rather than `<redacted>@`.

Verification: PASSED — `make test` (795 tests run, 795 passed, 0 failed; 7 leaky, which is the
expected scout-timeout behaviour documented in CLAUDE.md), plus `cargo fmt --all` and
`cargo clippy --workspace --all-targets` clean. The workspace doctests that `make test` runs
after nextest also passed.
