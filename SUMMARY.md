# Unseal-key custody through `keyring`: native OS stores instead of `/usr/bin/security`

One thing is not delivered and belongs up front: the issue's first acceptance
criterion — "an existing item created by `security add-generic-password` is
readable through `keyring` on macOS — verified, not assumed" — needs a Mac, and
this ran on Linux with no `/usr/bin/security` and no Security.framework. It is
still unverified; whoever first runs this on a Mac is the first to verify it,
and the fallback below exists so that answer cannot lock anyone out.

`crates/tasks/src/secrets.rs` now reads and writes the 32-byte unseal key
through the [`keyring`](https://crates.io/crates/keyring) crate's native
backends — Security.framework on macOS, the Credential Manager on Windows, the
Secret Service elsewhere — against the same service `tasks-v2-secrets` and the
same account `unseal`. `keychain_read` and `keychain_write` remain the whole
custody boundary and are still the only two functions in the repository that
touch an OS key store; everything else (`init`, the new `rehome_key`, the
server's `Secrets::open`) goes through them, and the `KEYCHAIN_SERVICE` doc
comment plus the `CLAUDE.md` rule both say so, so #1004's auto-initialise route
has somewhere to read that before it grows a second one. Nothing else moved:
the store format, `key_source: "keychain"`, the `--key-file` /
`TASKS_SECRETS_KEY_FILE` paths and every existing `tasks secrets` subcommand
behave as before, and there is no migration. `init` is still refused off macOS
without `--key-file`, deliberately un-widened even though the Secret Service is
now reachable. The whole crate rather than `keyring-core` plus a target-gated
store, so `secrets.rs` stays platform-independent source that Linux type-checks
and lints in full; version 4 and not 3, because keyring 3 with no platform
feature falls back to an in-memory *mock* whose `set_password` reports success
and loses the key, while keyring 4 answers `NoDefaultStore` — a failure this
module turns into a sentence naming `TASKS_SECRETS_KEY_FILE`.

**What this does not buy yet, stated in the rule and not only in a comment.**
A macOS access list is bound at creation and `set_password` is
find-then-modify-*in-place*, so an item the `security` CLI created keeps the
CLI's access list through any number of native writes; the `security` read
therefore survives as the **default** fallback on the read path (read-only —
there is no `security` write left — and deliberately not `cfg`-gated, so every
platform compiles the migration path); and an unsigned dev build is a different
*application* to an access list on every `cargo build`. For an existing install
custody is consequently **unchanged** until a human runs the new `tasks secrets
rehome-key`, and nothing forces that — a `warn!` naming the command is the only
prompt. `rehome-key` is delete-then-add, which is the only thing that moves an
access list and is destructive on the only copy of a live store's key, so the
order is: read (native, then the legacy fallback), validate the hex, park it in
a 0600 rescue file **outside the data dir**, delete, add, read back and compare
constant-time, prove the sealed store still decrypts under the read-back value,
and only then remove the rescue file. Every failure from the delete onwards
returns the rescue path, the `TASKS_SECRETS_KEY_FILE=` line that opens the store
with it, and the instruction to delete it once custody is settled. The key is
never printed, returned or logged.

## Review feedback

1. **Rescue file out of `<data dir>/secrets/`** — done, and out of the data dir
   entirely rather than just out of that subdirectory: `$HOME/.tasks/` (the
   #1012 service home, per-user, not what anyone backs up when they back up the
   server's state). A rescue copy anywhere under the data dir would put both
   halves of the two-artifact property in one `tar`, and the file is kept
   exactly when the rehome failed. `rescue_hint()` states the full path twice —
   once as a path, once inside the `TASKS_SECRETS_KEY_FILE=` line — and says in
   words that it must be deleted once custody is settled, because it is the
   other half of the property. A homeless environment refuses the rehome rather
   than falling back to the data dir. `tasks service uninstall` removes only the
   plist, so nothing sweeps `~/.tasks` out from under it.
2. **Pitfall 6's "visible in `tasks secrets status`" is false** — not shipped.
   The module header now says plainly that a default-keychain mismatch is *not*
   detected, why (`status` reads the store header, which says `keychain` either
   way, and is documented as needing no unseal key so it cannot probe one), and
   what the symptom looks like: a `rehome-key` whose delete reports no entry
   while its write appears to succeed. It also says which guard catches it —
   item 5's read-back comparison.
3. **Run the suite** — `make test` and `cargo clippy --workspace --all-targets`
   both ran here; the result is in the trailer. Clippy was clean with no
   warnings, and it is also what first compiled the two new `#[cfg(test)]`
   tests (`--all-targets` builds test targets, which `cargo build` does not);
   no compile fix turned out to be needed in `mod tests`.
4. **Say plainly what this does not buy yet, in `CLAUDE.md`** — done, in the
   credential-custody rule itself: the three compounding reasons, that custody
   is unchanged for an existing install until a human runs `rehome-key`, that
   nothing forces it, and that the real benefit arrives with a signed
   application identity (#988, undecided). The `TASKS_SECRETS_KEY_FILE` table
   row now says the key file is first-class on macOS too, not the exotic-host
   path.
5. **Read-back must compare the value** — done, `Secret::matches` (constant-time
   in the value bytes), and then one step further: the read-back bytes are used
   to derive the KEK and decrypt every entry in the store, so what is verified
   is "the sealed store opens under what the credential store now holds" rather
   than "a read succeeded". Both checks report the rescue path on failure. No
   third credential-store read is made, so the rehome costs at most two access
   prompts.

## Directions

- **Nothing here contains a credential.** No key, fragment, fixture or captured
  output; the two new tests use the key-file path and a literal store value that
  is not a credential, the rehome's read-back compares through `Secret` and never
  formats it, and no `Display` was added to `Secret`.
- **Base is newer than the spec (#1012), and it does change something.** Two
  findings, both written up above and in the module header. The rescue file's
  new home is `~/.tasks/`, which exists as a stable per-user location precisely
  because #1012 established it — and `tasks service uninstall` removes only the
  LaunchAgent plist, so it is safe to park a file there. Second, #1012 makes the
  access-list weakness *more* visible rather than less: the installed daemon is a
  copy at `~/.tasks/bin/tasks` that `tasks reload` replaces on every upgrade, and
  an unsigned binary is identified to a macOS access list by its own contents, so
  each upgrade is a new application to that list — the same re-prompt-per-rebuild
  problem the spec names for dev builds, now on the installed path too. A
  launchd-started server may have no window server to answer that prompt, which
  is exactly when the `security` fallback is what keeps it serving. Nothing in
  the spec's design needed changing; the conclusion is that `--key-file` /
  `TASKS_SECRETS_KEY_FILE` is the right recommendation for an installed daemon
  until #988, and that is what the docs now say.
- **Run the suite and report what actually happened** — done; see the trailer.
  This build ran both `make test` and clippy, which the spec's own scouting could
  not (its VM's block device failed), so the two new tests are compiled and run
  here for the first time.
- **`SUMMARY.md` is the PR body** — this is a report of what was done. One thing
  is deliberately left out and worth naming in the first paragraph's place: the
  issue's first acceptance criterion ("an existing item created by `security
  add-generic-password` is readable through `keyring` on macOS — verified, not
  assumed") **is still unverified**, for the same reason the spec gave. This ran
  on Linux with no `/usr/bin/security` and no Security.framework, so whoever
  first runs this on a Mac is the first to verify it. What is provable from the
  backend sources is the lookup: `apple-native-keyring-store`'s `keychain` store
  (which keyring's `v1` feature selects — *not* `protected`, the data-protection
  keychain, which would not see CLI-created items) uses the legacy
  `SecKeychain` generic-password API with the same `(service, account)`
  attributes the CLI uses. Whether the access list then permits the read is the
  half that needs a Mac, and the fallback exists so that answer cannot lock
  anyone out.
- **Departure from the issue, repeated here because the reviewer reads the
  issue**: the issue's criterion says the fallback should "rewrite through
  keyring". It ships as the separate `tasks secrets rehome-key` instead, because
  an automatic `set_password` would rebind nothing and an automatic
  delete-then-add is a destructive act on a live store's only key, running inside
  a server boot. The stated intent — an existing store is never locked out — is
  met by the read fallback alone.

Dependency cost on Linux, measured on this VM by resolving the workspace with
and without the dependency: `keyring = "4"` adds 53 crates (zbus,
secret-service, async-io/polling/rustix, num, aes/cbc, toml_edit …) to a
272-crate baseline, all pure Rust. `cargo build`, `cargo clippy` and `make test`
needed no new system packages and no D-Bus session; the D-Bus requirement is
runtime-only and only on the branch Linux never takes (a store header saying
`keychain`), where it surfaces as `NoDefaultStore` and the
`TASKS_SECRETS_KEY_FILE` sentence. The cost is compile time.

Beyond the suite, the CLI was exercised by hand on this Linux VM against
throwaway data dirs: `secrets init --key-file` / `set` / `status` behave as
before, `rehome-key` refuses a file-keyed store with the sentence the test pins,
and a hand-written `keychain`-headed store reproduces the read path's real
failure ("No default store has been set …; run `tasks secrets init`, or set
TASKS_SECRETS_KEY_FILE to a key file") promptly, with no D-Bus session present
and no hang.

Verification: PASSED — make test (cargo-nextest: 870 tests run, 870 passed, 0 skipped; plus `cargo test --doc --workspace`), and `cargo clippy --workspace --all-targets` clean
