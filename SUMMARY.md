# Tell every agent prompt that a piped command reports the pipe's exit status

gpuikit#180 was filed from three Scout runs that hit the same linker OOM, and one of
them **reported a green result on a build that had died**: the command was
`cargo test --all-features 2>&1 | tail -40`, so the shell reported `tail`'s exit status
and the kill read as exit 0. Nothing in this repo warned an agent about that —
`grep -rn 'pipefail\|PIPESTATUS' crates/ images/` returned nothing. This adds one clause
to the server-written agent prompts, beside the two that already live there for the same
reason: the run budget (#982) and a backgrounded command dying with the turn (#962). It
is the third clause of that kind and the worst of the three, because the other two lose a
run while this one produces a **false pass** — the agent believes the suite was green and
reports it that way, and everything downstream believes the agent.

Unlike those two it is stated **once**, as `crate::prompt::PIPE_EXIT_STATUS` in a new
private `crates/tasks/src/prompt.rs`, and spliced verbatim into four prompts: the Scout's
(with step 3, above the `## Two things` heading, which counts its own contents), the
Builder's (after the supervisor's-suite paragraph), the worker's (after `Budgets:`) and
the orchestrator's. The budget clause beside it differs per run because the fact differs;
this one is a fact about `sh`, identical on every host, so there is nothing to say in two
voices and a second copy could only rot. The clause refuses to name `set -o pipefail`
alone — it is a bashism, and an agent that tries it under `sh` gets
`Illegal option -o pipefail` and learns the advice is wrong — and instead names three
escapes in order, ending with the one that always works: do not pipe it, redirect to a
file and read the file. Host-side prompt text: effective on a server restart, no image
rebuild, and `images/` is untouched. Tests are one wording test on the const (each escape
by name, plus their order) and one presence test per prompt asserting the whole const, so
a paraphrase or a dropped splice goes red rather than a keyword match passing on either.

## Review feedback

- **Add the orchestrator, unconditionally, and not to `verification_section`.** Done —
  `crates/tasks/src/orchestrator.rs::system_prompt` splices the same const between the
  workdir and verification sections, from a local bound outside any conditional. The test
  renders the prompt twice, with `target_dir` set and unset, precisely because
  `verification_section` returns `String::new()` in the second case: that is the host that
  cannot verify and must reason hardest from command output. The diagnosing-a-red-check
  example (`… --log-failed 2>&1 | grep … | head -6` returning nothing, with two readings
  available) and the independent gpuikit-scout corroboration are both recorded in the
  const's doc comment beside the gpuikit#180 provenance, as asked.
- **Pin the count in the wording test, not just the words.** Done, and the test is renamed
  `the_pipe_clause_names_three_escapes_ending_with_the_one_that_works_under_sh` (the spec
  called it `…names_both_escapes…`, which said *both* of *three*). It asserts each escape
  by name — `set -o pipefail` together with the `bash only` caveat that keeps it from
  teaching the wrong lesson, `${PIPESTATUS[0]}` with its braces and its index, and
  redirect-to-a-file — and then asserts their byte offsets are in that order, so the
  ordering argument is pinned rather than left as a comment.

## Directions

- **Do not fix `clippy::result_large_err` in `broker.rs`.** Not touched; the base already
  carries it (trunk `e926d57` is in this branch's history via `a9730f8`).
- **#1077 and #1080 own no file here.** Confirmed — the change touches
  `crates/tasks/src/{prompt,lib,scout,builder,worker,orchestrator}.rs` and nothing under
  `crates/builder-supervisor/` or `images/`. No collision and no workaround needed.
- **Do not read an exit status through a pipe while verifying this.** Followed:
  `sh .tasks/verify > /tmp/verify.log 2>&1; echo "EXIT=$?"` — redirect and then read the
  file, which is the third escape the clause itself recommends. No `tee`, no `grep`, no
  `pipefail` needed because no pipe was used on a status-bearing command.

## One pre-existing test adjusted

`orchestrator::tests::the_prompt_asks_for_the_config_file_and_never_a_shell_variable`
asserted `!p.contains('$')` over the whole prompt — a blunt proxy for its stated intent,
that the actor credential is presented via the `-K` config file rather than interpolated
from `$TASKS_ACTOR_TOKEN`, which Claude Code refuses to run under a static
`Bash(curl:*)` allowlist. `${PIPESTATUS[0]}` is the first legitimate `$` this prompt has
carried. Rather than delete the assertion, the shared const is excised from the rendered
prompt and the blanket check runs over everything this file writes itself, with an
`assert_ne!` guarding that the excision was not a silent no-op — so a `$FOO` added
anywhere else still goes red, and the const has its own test. The interaction is noted in
the const's doc comment too: under a static allowlist an agent cannot run a pipeline at
all, and the clause ends on the escape that needs no variable.

Verification: `sh .tasks/verify` (`make test-ci` plus doctests) exits 0 —
**1142 tests run, 1142 passed**, 0 skipped, 3 known-leaky; `cargo clippy --workspace
--all-targets` and `cargo fmt --all --check` clean.

---

(`PROMPT.md`, this run's own prompt written into the repo root by the harness, was never tracked on the base and is not part of this change; it was removed from the working tree so the packaging sweep does not add it to the diff.)
