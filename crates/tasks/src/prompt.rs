//! Prompt prose shared by more than one agent, stated once.
//!
//! There is exactly one thing here, and the bar for a second is the same one
//! it cleared: a fact with **no per-run parameter in it**. The two clauses
//! that live beside it in the individual prompts — the run budget (#982) and
//! a backgrounded command dying with the turn (#962) — are hand-written per
//! file, and for the budget that is not an oversight: it is rendered from the
//! budget *that* run was given, so the sentence differs because the fact
//! differs. Nothing here may take an argument, or the argument for one source
//! evaporates and this becomes a worse copy of a `format!`.

/// What `sh` reports for a pipeline, said to every agent that reads an exit
/// status.
///
/// gpuikit#180 was filed from three Scout runs that hit the same linker OOM,
/// and one of them **reported a green result on a build that had died**: the
/// command was `cargo test --all-features 2>&1 | tail -40`, so the shell
/// reported `tail`'s status and the kill read as exit 0. Nothing in this repo
/// warned an agent about that — `grep -rn 'pipefail\|PIPESTATUS' crates/
/// images/` returned nothing.
///
/// It is the third clause of its kind and the worst of the three, because the
/// other two lose a run while this one produces a **false pass**: the agent
/// believes the suite was green and reports it that way, and everything
/// downstream believes the agent. In the orchestrator's turn that belief ends
/// in `POST /pull-requests/{n}/merge`, whose documented recourse is a revert —
/// which is why the clause is unconditional there rather than living in
/// `verification_section`, a function that returns nothing on precisely the
/// hosts that cannot verify and must therefore reason hardest from command
/// output.
///
/// The corroboration is two agents in two repositories inside a day. The
/// review session that approved this spec ran
/// `gh run view --log-failed 2>&1 | grep -iE "error\[|error:" | head -6` while
/// diagnosing a red check, got no output at all, and had two readings
/// available — "the job logged no errors" and "one of three commands failed
/// and the last one was `head`". It re-ran with `tail` rather than concluding
/// the former, which is luck rather than method. Independently, a scout in
/// gpuikit writing that repository's `.tasks/verify` on the same day
/// rediscovered the defect from the other end and wrote "a pipeline reports
/// its *last* command's status, which is how an OOM-killed link comes back as
/// success" into its own reasoning.
///
/// Naming `set -o pipefail` alone would be a trap: it is a bashism, so an
/// agent that tries it under `sh` gets `Illegal option -o pipefail` and learns
/// that the advice is wrong. Three escapes, ordered, ending with the one that
/// works in every shell — which is also the escape that survives a static
/// `--allowedTools` allowlist, where a command carrying a shell variable is
/// not statically verifiable and an agent so restricted cannot run a pipeline
/// anyway. Host-side prompt text: it reaches agents on the next server
/// restart, and `make images` is not part of shipping it.
pub(crate) const PIPE_EXIT_STATUS: &str = "\
    **A piped command does not report its own exit status.** The shell reports \
    the status of the LAST command in the pipeline, so `cargo test … 2>&1 | \
    tail -40` exits 0 when the build was killed and only `tail` succeeded — a \
    dead run reads as a green one, and nothing downstream can tell. Three ways \
    out, and the last is the only one that always works: `set -o pipefail` \
    before the pipeline (bash only — under `sh` it is an error, so do not \
    reach for it blind); `${PIPESTATUS[0]}` after it (the braces and the index \
    both matter — bare `$PIPESTATUS` is the first element on bash and nothing \
    at all elsewhere); or do not pipe it — redirect the output to a file, let \
    the command's own status be the shell's, and read the file afterwards.";

#[cfg(test)]
mod tests {
    use super::PIPE_EXIT_STATUS;

    /// Each escape by name, and the count pinned rather than the word "both":
    /// if the const later loses one, a test asserting three named strings goes
    /// red where a keyword match would not. The ordering is the point — the
    /// redirect is last because it is the one that works under `sh`, where the
    /// bashism above it is an error.
    #[test]
    fn the_pipe_clause_names_three_escapes_ending_with_the_one_that_works_under_sh() {
        assert!(
            PIPE_EXIT_STATUS.contains("set -o pipefail"),
            "the first escape must be named: {PIPE_EXIT_STATUS}"
        );
        assert!(
            PIPE_EXIT_STATUS.contains("bash only"),
            "naming pipefail without saying it is a bashism teaches an agent \
             the advice is wrong: {PIPE_EXIT_STATUS}"
        );
        assert!(
            PIPE_EXIT_STATUS.contains("${PIPESTATUS[0]}"),
            "the second escape needs its braces and its index: {PIPE_EXIT_STATUS}"
        );
        assert!(
            PIPE_EXIT_STATUS.contains("redirect the output to a file"),
            "the third escape — the only one that always works — must be \
             named: {PIPE_EXIT_STATUS}"
        );
        let pipefail = PIPE_EXIT_STATUS.find("set -o pipefail").unwrap();
        let pipestatus = PIPE_EXIT_STATUS.find("${PIPESTATUS[0]}").unwrap();
        let redirect = PIPE_EXIT_STATUS
            .find("redirect the output to a file")
            .unwrap();
        assert!(
            pipefail < pipestatus && pipestatus < redirect,
            "the escape that works everywhere comes last: {PIPE_EXIT_STATUS}"
        );
    }
}
