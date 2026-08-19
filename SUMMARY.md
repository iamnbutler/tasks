# `.env.example`, and the three variables in the working `.env` that nothing reads

Two halves, one of which is a finding rather than a code change.

**The example.** `.env.example` is new at the repository root — the file
`.gitignore:6`'s `!.env.example` negation has been anticipating — and covers
only the six variables a person actually has to decide about (`GITHUB_TOKEN`,
`ANTHROPIC_API_KEY`, `TASKS_DEFAULT_MODE`, `SCOUT_MAX_CONCURRENT`,
`ORCHESTRATOR_CMD`, `ORCHESTRATOR_WORKDIR`), deferring everything else to
CLAUDE.md's table. Every line is commented out, so an unedited copy changes
nothing. The credential section leads with `tasks secrets init` /
`tasks secrets set github-token` / `set anthropic-api-key` and presents the two
raw variables *below* that, labelled as fallbacks and noting that the server
warns at startup for each one it falls back to — the sealed store is where
production keys live, and a starter file that taught the opposite would be #971
being undone at the front door. The GitHub scopes are derived from the actual
write surface in `crates/tasks/src/github.rs` (create/merge/close a PR,
create/close/reopen/update an issue, `set_issue_labels`, issue and review
comments, plus the branch push) and stated in both vocabularies: classic `repo`;
fine-grained Contents RW + Pull requests RW + Issues RW + Metadata R. The
Anthropic entry states the full resolution order — sealed store → this variable
→ the host's `~/.claude/anthropic_key.sh` `apiKeyHelper` (`secrets.rs`'s
`env_fallbacks`) — because that last step is the genuinely invisible one: with
the helper present, everything works with nothing set and nothing saying why,
which is what makes the second machine a mystery. Both orchestrator shapes are
shown under labelled `(A)` / `(B)` headings, quoted (the values contain spaces
and `dotenvy` strips the quotes), with `--dangerously-skip-permissions`
described as *discarding* the allowlist rather than widening it.

**Two guard tests** are appended to `mod tests` in `crates/tasks/src/env_file.rs`
— the module that owns `.env` parsing, so they sit beside the loader whose
behaviour they depend on. `every_variable_the_example_names_is_known_to_the_tree`
extracts every `NAME=` from the example (commented-out lines included — that is
every line, and a dead variable hides in a comment exactly as well as in a live
assignment) and asserts each name appears somewhere under `crates/`, `images/`
or the `Makefile`. It is a naming check and not a proof of use, which its doc
comment says out loud; that is deliberately the same bar the issue applied by
hand with `grep -rn`. **The check is one-directional on purpose, and the
argument is in the test's doc comment**: the reverse direction (every variable
the tree reads is documented) has no grep-shaped definition — `env::var` calls
are wrapped, names are built from constants, and test fixtures set variables no
operator should — and a guard that fires on things that are fine gets deleted,
taking the direction that *can* be made precise with it.
`the_examples_orchestrator_command_survives_being_uncommented` uncomments the
assignment lines, runs them through the module's own `parse`, and asserts both
commands come back starting `claude --print ` with `Bash(curl:*)` and
`--dangerously-skip-permissions` intact — that no quote leaked into what would
become the program name. Both were verified falsifiable: appending
`# TASKS_CONTAINER_IMAGE=agent:v1` to the example turns the first red, and
deleting one orchestrator shape turns the second red. That first check caught a
real defect in its own first draft — the guard's doc comment names the three
dead variables, and since the walk read `env_file.rs`, the test was satisfied by
its own prose. The searchable tree now excludes the file holding the guard, and
says why.

## The finding: all three variables are dead, and they are in two files

`TASKS_MAX_SESSIONS`, `TASKS_DISPATCH_INTERVAL` and `TASKS_CONTAINER_IMAGE` have
zero occurrences anywhere in the tree and — the part the issue did not know —
zero occurrences anywhere in this repository's git history (`git log --all -S`
per name). None was renamed or removed here; they are v1 residue in an untracked
local file, and the nearest true statement for `TASKS_CONTAINER_IMAGE` is that
its *concept* was split into two differently-named variables, `SCOUT_IMAGE` and
`BUILDER_IMAGE` (`run.rs:83-84`).

| var | in tree | in git history | verdict |
| --- | --- | --- | --- |
| `TASKS_MAX_SESSIONS` | none | none | never existed here |
| `TASKS_DISPATCH_INTERVAL` | none | none | never existed here |
| `TASKS_CONTAINER_IMAGE` | none | none | never existed here |

Because `.env` is gitignored, deleting them is a host-side act no Builder can
perform. It is **two files, not one**, and both must be cleaned:

- `<checkout>/.env`
- `<data dir>/.env` — `~/.local/state/tasks-v2/.env`

The data-dir one is the one that matters, and the reason is CLAUDE.md's load
order: it is launcher-independent and the only file an installed binary outside
a checkout can have, so it is what still feeds a launchd-started server after
`tasks service install` and after the checkout is gone. (The two are genuinely
different files rather than a copy — only the checkout's carries
`TASKS_DATA_DIR`.) `env_file::report` already logs `loaded .env` with the full
list of variable names it applied, so `serve.log` is the before/after for both;
nothing else observes these three, so there is no other effect to check.

## Review feedback

1. **Lead with the sealed store; demote both credentials to commented
   fallbacks; adjust the README so step one is not putting secrets in a file.**
   Done, and it changed the shape of the file rather than a line in it. The
   credential section opens with the three `tasks secrets` commands, notes that
   a sealed key is picked up by a running server with no restart and never
   reaches a VM, and then presents `GITHUB_TOKEN` / `ANTHROPIC_API_KEY`
   commented out and labelled as fallbacks, with the startup warning named. The
   scope derivation from `github.rs` is kept verbatim in intent. In the README,
   `cp .env.example .env` goes *after* the `tasks secrets` lines, described as
   "everything that is *not* a credential" — this overrides the spec, which put
   it first.
2. **Argue the guard's one-directional scope in the spec rather than leaving it
   a gap; do not fix `BUILDER_IMAGE` here.** Done — the argument is in
   `every_variable_the_example_names_is_known_to_the_tree`'s doc comment and in
   this summary. `BUILDER_IMAGE`'s missing CLAUDE.md row is untouched; CLAUDE.md
   is not modified at all by this change.
3. **Name both `.env` paths, say both must be cleaned, give the load order as
   the reason the data-dir one matters.** Done, in the finding above. This
   corrects the spec, which treated it as one file.

## Directions

- *Account for all three required review changes in `SUMMARY.md`, declines
  included.* Done above; none declined.
- *The first one changes the shape of the file, not a line in it.* Taken that
  way — see item 1.
- *The third is a factual correction: two files, with the load order as the
  reason.* Taken; the finding names both paths and the reason.
- *Do not fix `BUILDER_IMAGE`'s missing CLAUDE.md row (#1023).* Honoured —
  CLAUDE.md is not in the diff.
- *`README.md` is contended: your two lines go in the Running-it block, touch
  nothing else, run no formatter.* Honoured. The diff to `README.md` is one
  added line (`README.md:72`, `cp .env.example .env` inside the Running-it
  fenced block) and one reflowed sentence in the credentials paragraph
  (`README.md:90-91`, adding the clause pointing at the file). No formatter was
  run over the file; nothing else in it is touched.
- *Watch the shape of the example — a long file nobody reads is worse than the
  lines that matter.* Partially declined, as the spec also flagged: the file is
  89 lines for 6 variables (7 assignment lines, since both orchestrator shapes
  are shown). Requirement 1 grew it — the secrets-first lead is ~15 lines that
  the original uncommented-credentials shape did not need — and the rest is the
  material the issue explicitly asked for: token scopes, the `apiKeyHelper`
  precedence, both orchestrator shapes labelled, the memory ledger behind
  `SCOUT_MAX_CONCURRENT`, and why boot-paused surprises people. The count that
  the direction's intent was about is six variables, not thirty. If a reviewer
  wants it shorter, the compressible part is still the orchestrator section.

One deviation worth flagging that neither the spec nor the feedback covers: the
spec's `.env.example` had 6 assignment lines, this one has 7, because
`ORCHESTRATOR_CMD` appears once under each of `(A)` and `(B)`. That is what the
second guard test asserts, and it is why the test counts two commands rather
than one.

Verification: PASSED — `make test` (952 tests across 40 binaries, 952 passed, 0 failed; the 7 LEAK results are the documented expected ones) plus doctests, `cargo clippy --workspace --all-targets` (exit 0, zero warnings) and `cargo fmt --all --check` clean.
