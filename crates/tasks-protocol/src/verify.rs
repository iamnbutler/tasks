//! Whether a passing run of the project's own test suite backs a build.
//!
//! The thing this replaces was a sentence: `Verification: PASSED|FAILED|NOT
//! RUN`, written by the Builder agent into `SUMMARY.md` and grepped by the
//! host. It gated a write to GitHub on prose the graded party authored, which
//! is the defect [`crate::FailureClass`] already forbids one level up — a
//! decision that greps text changes meaning the next time somebody improves a
//! sentence, and here the somebody is the agent being judged.
//!
//! So the Builder supervisor runs the suite itself, inside the VM, against the
//! swept tree the bundle carries, and stamps a [`Verification`] on
//! `BuildEvent::Completed`. The host branches on [`Verification::status`] and
//! only ever *renders* [`Verification::detail`] — the deliberate sibling of
//! `class: FailureClass`, and for the same reason.
//!
//! # There is no `Failed`
//!
//! [`VerificationStatus`] has no red variant, so "shipped and red" is
//! unrepresentable rather than merely avoided: a suite that fails does not
//! produce a status at all, it fails the build inside the VM before a bundle
//! is packaged. Every state that *is* representable is a state in which no
//! passing run backs the batch, and [`VerificationStatus::is_green`] is the
//! only way anything asks — so a variant added later cannot become green by
//! omission.
//!
//! # This module is the spawn-free half
//!
//! The types, the script path and the budget arithmetic live here; running
//! anything lives in `builder-supervisor`. That is the split
//! [`crate::agent_run`] and [`crate::vm_memory`] already set, and it is what
//! makes the arithmetic testable without a VM.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

/// Where a repository declares how it is tested, read out of the build's
/// **base** commit and never out of its tip.
///
/// A script rather than a command string: no parser, no schema to version, a
/// multi-step suite is one file, and it sidesteps the quoting trap CLAUDE.md
/// documents, where a command split on whitespace shatters `Bash(git log:*)`
/// into two permissions that match nothing. Always invoked as `sh <script>` —
/// the shebang is decorative and the executable bit is deliberately not
/// consulted, because honouring it would mean two invocation paths that can
/// drift and a mode bit a `git apply` can drop silently.
pub const VERIFY_SCRIPT_PATH: &str = ".tasks/verify";

/// Seconds of the run budget held back for packaging the bundle after the
/// suite has run.
///
/// The inner suite budget is sized to expire **first**, and that ordering is
/// what keeps the outer `BUILDER_TIMEOUT_SECS` expiry defensible as a
/// `Verdict`: a build that runs its budget to zero really did run to
/// completion and produce nothing, rather than losing a coin toss about which
/// clock fired.
pub const PACKAGING_RESERVE_SECS: u64 = 120;

/// Floor under the derived suite budget. Below this there is no point
/// starting: a suite that cannot finish in a minute cannot finish in ten
/// seconds either, and the floor is what stops a nearly-spent run from
/// reporting a timeout it never gave the suite a chance to avoid.
pub const MIN_SUITE_BUDGET_SECS: u64 = 60;

/// Operator override for the derived budget, read inside the VM.
///
/// A hard ceiling for a project whose suite needs one, and `0` to skip the
/// suite outright. A legitimate knob rather than a test hook — though it is
/// also the only thing that makes the kill-during-the-suite path testable in
/// seconds, since the derivation floors at [`MIN_SUITE_BUDGET_SECS`].
pub const SUITE_BUDGET_VAR: &str = "BUILDER_SUITE_BUDGET_SECS";

/// What a build's own test run said — or, in every variant but one, why there
/// is nothing it said.
///
/// Read the module docs for why there is no red variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// The project's declared suite ran to completion and passed. The only
    /// green state there is.
    Passed,
    /// The project declares no suite at [`VERIFY_SCRIPT_PATH`], or declares an
    /// empty one.
    ///
    /// A project that declares nothing dispatches ungated and reports this —
    /// refusing to dispatch would wedge a repository on a convention it has
    /// not adopted, and "absence of evidence never holds" is this codebase's
    /// standing rule across every dispatch hold. Never green, so a batch
    /// carrying it routes to a human exactly as it did before the check
    /// existed.
    Undeclared,
    /// Something below the suite failed: a runner that would not start, a
    /// script that could not be read or staged, a budget the host never
    /// stated, or — on the host side — a status this binary could not parse.
    ///
    /// This is also where an unknown wire value decays to, which is why it can
    /// never be read as anything but "no passing run backs this".
    Unavailable,
    /// The suite was killed by its budget, or there was not enough budget left
    /// to start it.
    ///
    /// Deliberately **not** a failure of the build: a suite that never
    /// finished is not evidence about the work, the implementation may be
    /// perfect, and throwing it away because a cold `target/` compiled slowly
    /// is the failure #929 and #884 were filed about. The branch ships and the
    /// status is honestly not green.
    TimedOut,
}

impl VerificationStatus {
    /// The wire form, and what a log line prints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Undeclared => "undeclared",
            Self::Unavailable => "unavailable",
            Self::TimedOut => "timed_out",
        }
    }

    /// Whether a passing run of the project's own suite backs this build.
    ///
    /// The **only** way anything asks. A `matches!(status, Passed)` written at
    /// a call site is a second answer to this question that a variant added
    /// tomorrow can silently fall through; this one cannot, because every
    /// variant has to be named here.
    pub fn is_green(&self) -> bool {
        match self {
            Self::Passed => true,
            Self::Undeclared | Self::Unavailable | Self::TimedOut => false,
        }
    }

    /// The wire form, read forgivingly. See the [`Deserialize`] impl.
    fn from_wire(raw: &str) -> Self {
        match raw {
            "passed" => Self::Passed,
            "undeclared" => Self::Undeclared,
            "timed_out" => Self::TimedOut,
            _ => Self::Unavailable,
        }
    }
}

impl std::fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unknown decays to [`VerificationStatus::Unavailable`], never toward green.
///
/// Hand-written for the reason [`crate::FailureClass`]'s impl is: a *newer*
/// supervisor sending a status this binary has never heard of must not make
/// the terminal event undecodable, because a lost terminal event does not cost
/// a strike — it costs the run its outcome and hangs it until the deadline.
/// The direction of the decay is the other half: `Unavailable` is "no passing
/// run backs this", which is what an unrecognised answer honestly is.
///
/// Reading the value as a `serde_json::Value` first means a status sent as a
/// number or a null decays the same way a misspelt string does.
impl<'de> Deserialize<'de> for VerificationStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(raw
            .as_str()
            .map(VerificationStatus::from_wire)
            .unwrap_or(VerificationStatus::Unavailable))
    }
}

/// A build's verification, as the supervisor stamps it.
///
/// `detail` is prose for a human and **nothing branches on it** — the same
/// contract `FailureClass`'s `reason` has. It always names the gate that
/// ruled (the blob SHA of the `.tasks/verify` that ran), because a field that
/// appears only on disagreement is one nobody learns to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub status: VerificationStatus,
    #[serde(default)]
    pub detail: String,
}

impl Verification {
    pub fn new(status: VerificationStatus, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    /// Shorthand for [`VerificationStatus::is_green`].
    pub fn is_green(&self) -> bool {
        self.status.is_green()
    }
}

/// How much time the suite gets, or why it does not run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuiteBudget {
    /// Run the suite, killing it after this long.
    Run(Duration),
    /// Do not run it; report this instead. Never green.
    Skip(Verification),
}

/// Size the suite's budget against what is left of the run's.
///
/// `remaining_secs` is what the host said was left of the build budget when it
/// dispatched (`None` from a host too old to say). `cap_secs` is
/// [`SUITE_BUDGET_VAR`], which overrides the derivation outright.
///
/// The derived budget is the remainder minus [`PACKAGING_RESERVE_SECS`],
/// floored at [`MIN_SUITE_BUDGET_SECS`] — sized to expire before the outer
/// deadline does, because a build killed by *its* budget is a `Verdict` and a
/// suite killed by *this* one costs the build nothing.
///
/// Two skips, and they are different facts rather than one rounded off:
/// less run budget than packaging alone needs is a [`VerificationStatus::TimedOut`]
/// (there was not enough to start), while a host that stated no budget at all
/// is [`VerificationStatus::Unavailable`] (we do not know, and guessing an
/// hour would hand the suite time the host will not honour).
pub fn suite_budget_secs(remaining_secs: Option<u64>, cap_secs: Option<u64>) -> SuiteBudget {
    if let Some(cap) = cap_secs {
        if cap == 0 {
            return SuiteBudget::Skip(Verification::new(
                VerificationStatus::Unavailable,
                format!("{SUITE_BUDGET_VAR} is 0, so this VM ran no suite at all"),
            ));
        }
        return SuiteBudget::Run(Duration::from_secs(cap));
    }
    let Some(remaining) = remaining_secs else {
        return SuiteBudget::Skip(Verification::new(
            VerificationStatus::Unavailable,
            "the host did not say how much run budget was left, so there was no bound to run \
             the suite under (the server predates this field — restart it)",
        ));
    };
    if remaining <= PACKAGING_RESERVE_SECS {
        return SuiteBudget::Skip(Verification::new(
            VerificationStatus::TimedOut,
            format!(
                "only {remaining}s of run budget remained, which is less than the \
                 {PACKAGING_RESERVE_SECS}s held back to package the bundle, so the suite was \
                 never started"
            ),
        ));
    }
    SuiteBudget::Run(Duration::from_secs(
        (remaining - PACKAGING_RESERVE_SECS).max(MIN_SUITE_BUDGET_SECS),
    ))
}

/// Read [`SUITE_BUDGET_VAR`]. An unparseable value is `None` — the derivation,
/// which is the safe direction: a typo must not switch the gate off.
pub fn suite_budget_cap_from_env() -> Option<u64> {
    std::env::var(SUITE_BUDGET_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the enum's shape: there is no way to say "shipped
    /// and red", and nothing but `Passed` reads as backed by a run.
    #[test]
    fn exactly_one_status_is_green() {
        assert!(VerificationStatus::Passed.is_green());
        for status in [
            VerificationStatus::Undeclared,
            VerificationStatus::Unavailable,
            VerificationStatus::TimedOut,
        ] {
            assert!(!status.is_green(), "{status} must not read as green");
        }
    }

    #[test]
    fn statuses_round_trip() {
        for status in [
            VerificationStatus::Passed,
            VerificationStatus::Undeclared,
            VerificationStatus::Unavailable,
            VerificationStatus::TimedOut,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: VerificationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status, "{json}");
        }
    }

    /// A newer supervisor's unknown status must decode — a terminal event that
    /// will not parse hangs the run until the deadline — and must decay away
    /// from green, never toward it.
    #[test]
    fn an_unknown_status_decays_to_unavailable_rather_than_failing_the_decode() {
        for raw in [r#""wildly_green""#, "17", "null", r#""passed ""#] {
            let status: VerificationStatus = serde_json::from_str(raw).unwrap();
            assert_eq!(status, VerificationStatus::Unavailable, "{raw}");
            assert!(!status.is_green(), "{raw}");
        }
    }

    /// The forged spelling that matters: nothing that is not exactly `passed`
    /// may come back green.
    #[test]
    fn only_the_exact_wire_word_reads_as_passed() {
        for raw in [r#""PASSED""#, r#""pass""#, r#""Passed""#, r#""green""#] {
            let status: VerificationStatus = serde_json::from_str(raw).unwrap();
            assert!(!status.is_green(), "{raw} must not read as green");
        }
        let real: VerificationStatus = serde_json::from_str(r#""passed""#).unwrap();
        assert!(real.is_green());
    }

    #[test]
    fn a_verification_round_trips_with_its_detail() {
        let v = Verification::new(VerificationStatus::Passed, "make test-ci (gate abc1234)");
        let json = serde_json::to_string(&v).unwrap();
        let back: Verification = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    /// A supervisor that sends only a status still decodes: `detail` is prose
    /// and its absence is not a reason to lose the terminal event.
    #[test]
    fn a_verification_without_a_detail_decodes() {
        let v: Verification = serde_json::from_str(r#"{"status":"undeclared"}"#).unwrap();
        assert_eq!(v.status, VerificationStatus::Undeclared);
        assert!(v.detail.is_empty());
    }

    #[test]
    fn the_derived_budget_reserves_packaging_and_expires_before_the_run_does() {
        // An hour, the default build budget: the suite gets everything but the
        // packaging reserve, and strictly less than the run has left.
        assert_eq!(
            suite_budget_secs(Some(3600), None),
            SuiteBudget::Run(Duration::from_secs(3480))
        );
        // Tight, but past the reserve: the floor applies, and the floor is what
        // makes it worth starting at all.
        assert_eq!(
            suite_budget_secs(Some(150), None),
            SuiteBudget::Run(Duration::from_secs(MIN_SUITE_BUDGET_SECS))
        );
    }

    /// Less run budget than packaging alone needs is a timeout that never
    /// started, and it is never confused with a host that said nothing.
    #[test]
    fn the_two_skips_are_different_facts() {
        let SuiteBudget::Skip(too_late) = suite_budget_secs(Some(30), None) else {
            panic!("30s of budget must not start a suite");
        };
        assert_eq!(too_late.status, VerificationStatus::TimedOut);

        let SuiteBudget::Skip(unstated) = suite_budget_secs(None, None) else {
            panic!("an unstated budget must not start a suite");
        };
        assert_eq!(unstated.status, VerificationStatus::Unavailable);

        assert!(!too_late.is_green() && !unstated.is_green());
    }

    #[test]
    fn the_override_wins_outright_and_zero_skips() {
        assert_eq!(
            suite_budget_secs(Some(3600), Some(5)),
            SuiteBudget::Run(Duration::from_secs(5))
        );
        // Above the derivation too — it is a ceiling the operator sets, not a
        // clamp on ours.
        assert_eq!(
            suite_budget_secs(Some(200), Some(9000)),
            SuiteBudget::Run(Duration::from_secs(9000))
        );
        let SuiteBudget::Skip(off) = suite_budget_secs(Some(3600), Some(0)) else {
            panic!("0 must skip the suite");
        };
        assert_eq!(off.status, VerificationStatus::Unavailable);
        assert!(!off.is_green());
    }
}
