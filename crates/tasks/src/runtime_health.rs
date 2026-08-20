//! Whether this host can start a container at all, and whether dispatch should
//! wait until it can.
//!
//! On the morning of 2026-08-19 the container runtime was not running —
//! `apiserver is not running and not registered with launchd`, so it had not
//! survived a reboot. Dispatch resumed anyway and, in one play window, charged
//! an attempt to everything it could reach: **3 builds failed** on `allocate
//! failed: runtime error: transport closed before Ready` and **12 tasks were
//! charged one of their three dispatch attempts**. Nothing was wrong with any
//! of the work, and vm-pool itself was healthy and current — it accepted every
//! allocate and only then discovered it could not start a container (#1017).
//!
//! At twelve tasks a window, that is three windows from rejecting the whole
//! queue. The asymmetry the GitHub hold rests on applies unchanged: a false
//! hold costs one tick of latency and loses nothing, a false dispatch costs
//! one of three attempts on work that did nothing wrong.
//!
//! ## Why the existing holds did not cover it
//!
//! [`crate::github_health`] covers GitHub, [`crate::updates`] a half-applied
//! upgrade, [`crate::broker_health`] the credential broker. None of them is
//! about the substrate that actually runs the work, and
//! [`crate::pool_health`] is the closest and still the wrong subject: it asks
//! whether vm-pool has a **slot**, and a pool with every slot free answers
//! cheerfully while no container can be started at all.
//!
//! ## This is not what `ServiceErrorKind` decides, and that is deliberate
//!
//! #930 classifies a refused allocation so a caller can decide on a field
//! rather than on prose, and it waives a strike for `Capacity` alone —
//! `Runtime` and every other kind stays charged, because "a reference that
//! does not resolve refuses identically forever, and waiving that is the
//! retry-forever loop the cap exists to stop". That reasoning is right, and it
//! is why this module exists **instead of** widening that waiver: nothing here
//! touches `FailureClass::for_service_error`, because with the probe in place
//! the failing allocate never happens. Today's outage would have cost **zero**
//! attempts rather than twelve — which a strike waiver could not have
//! delivered, since a waived strike still spends a VM allocation, a pool slot
//! and a teardown per task, forever.
//!
//! ## The probe, and why it is not the allocate
//!
//! The tempting evidence is the refusal the dispatchers already collect. It
//! closes a circle: the natural clearing signal for a refusal-driven record is
//! a *successful allocation*, which is the one thing a hold prevents — the
//! same argument that keeps [`crate::pool_health`] on a `status` round trip
//! rather than on a classified refusal.
//!
//! So the evidence is `container system status`, which is what apple/container
//! documents as "checks whether the container services are running" and what
//! [`crate::doctor`] already asks. Two consequences follow, and the second is
//! the one worth having:
//!
//! - It needs **no protocol change and no vm-pool restart**. A field on
//!   `pool_status` would have been the other honest design and is inert until
//!   the pool that reports it is restarted — which, during an outage that a
//!   reboot caused, is exactly when nobody has restarted anything.
//! - It observes the fault **before the first allocate**, so the cost of an
//!   outage is a log line rather than a strike. A record written from refusals
//!   can only ever be one task late.
//!
//! ## Which answers hold
//!
//! The three rules again, and the first one is doing real work here:
//!
//! **Absence of evidence never holds.** A host with no `container` on `PATH`
//! is not a broken host — vm-pool can be built on `SupervisorRuntime`, the
//! test harnesses are, and a Linux checkout has no apple/container at all. A
//! [`Probe::Missing`] therefore touches nothing, exactly as
//! [`crate::broker_health`] refuses to hold on an unreachable bridge gateway.
//! A probe that **timed out** touches nothing either: it is not an answer, and
//! `doctor` reads the same outcome as a `Skip` for the same reason.
//!
//! **Only a fresh success clears one** — a zero exit from `container system
//! status`, and nothing else.
//!
//! **A hold nobody refreshes expires.**
//!
//! What holding buys is both halves of the cost, as everywhere else in this
//! family: the strike is not charged because the run never starts, and the VM
//! is not allocated, held and torn down either.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use tasks_api::http::RuntimeHold;

use crate::doctor::Probe;

/// The tool, and the subcommand apple/container documents as "checks whether
/// the container services are running".
///
/// Stated once here rather than at the call site, because `doctor` spells the
/// same pair and a reader comparing the two should find one string each.
pub const RUNTIME_PROBE: (&str, [&str; 2]) = ("container", ["system", "status"]);

/// How often the record is refreshed.
///
/// Longer than [`crate::broker_health`]'s, because this probe is a subprocess
/// rather than a TCP connect and the fault it finds is operator-scale — a
/// reboot, a `container system stop`. Half a minute of latency after someone
/// runs `container system start` is not a cost worth a spawn every tick.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// How long the probe waits before reporting that it did not learn the answer.
///
/// Shorter than `doctor`'s ten seconds for the reason `broker_health`'s is: a
/// gate awaiting this stalls the dispatch tick, and a timeout is not an answer
/// either way, so patience buys nothing here.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a record stays evidence without a refresh.
pub const STALE_AFTER: Duration = Duration::from_secs(300);

/// The invariant is a *relationship* between the two, not two knobs.
const _: () = assert!(STALE_AFTER.as_secs() >= 4 * PROBE_INTERVAL.as_secs());

/// A run of probes in which the container runtime was not running, not yet
/// ended by one where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Down {
    /// The first failed probe in this run. **Not** moved by later ones.
    pub since: DateTime<Utc>,
    /// The most recent failed probe, which keeps the record from going stale.
    pub last: DateTime<Utc>,
    /// How many probes have failed since `since`.
    pub probes: u32,
    /// What `container system status` said — prose for a reader, and the one
    /// thing that distinguishes a stopped service from a broken install.
    pub error: String,
}

impl Down {
    /// The sentence the event log and `/status` get.
    ///
    /// It names its own discharge, like every other hold's report: the fix is
    /// one command, and the reader's next question is always what to run.
    pub fn describe(&self) -> String {
        format!(
            "the container runtime is not running since {} ({} probe(s); `container \
             system status` says: {}). Scout and build dispatch waits for it: nothing \
             here can start a VM, so work dispatched now would fail at the allocate and \
             be charged an attempt for it. Queued work stays queued and nothing is \
             charged. Discharge: `container system start`",
            self.since.to_rfc3339(),
            self.probes,
            self.error,
        )
    }

    /// The wire shape `/status` reports.
    pub fn to_hold(&self) -> RuntimeHold {
        RuntimeHold {
            since: self.since,
            last_seen: self.last,
            probes: self.probes,
            error: self.error.clone(),
        }
    }
}

/// What one probe did to the record — the edge, so a caller can announce it
/// exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing an announcement would be about: a running runtime while it was
    /// running, a further failed probe inside a hold already in force, or a
    /// probe that observed nothing either way.
    Unchanged,
    /// A hold just went on.
    Stopped(Down),
    /// A hold that was in force just came off.
    Started(Down),
}

/// Whether this host can start a container, as last probed.
///
/// One record, shared by both dispatchers and `/status`, so the three cannot
/// disagree about whether a hold is in force.
#[derive(Debug)]
pub struct RuntimeHealth {
    /// Set only by [`RuntimeHealth::unprobed`]. A record that never probes
    /// never observes, and one that never observes never holds — the first of
    /// the three rules, read as a construction rather than as a state.
    probes_disabled: bool,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    down: Option<Down>,
    /// When a probe was last *claimed* — not when it answered.
    probed_at: Option<DateTime<Utc>>,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeHealth {
    pub fn new() -> Self {
        Self {
            probes_disabled: false,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// A record that never probes and therefore never holds.
    ///
    /// For tests of the *other* gates. Not a convenience: this probe reads a
    /// real tool on the machine running the suite, so a developer whose
    /// container services happen to be stopped would otherwise watch every
    /// dispatch test in the tree fail for this module's reasons rather than
    /// its own — and a CI host with a healthy runtime would never reproduce
    /// it.
    ///
    /// **Structural rather than a pre-claimed probe**, for the reason
    /// [`crate::broker_health::BrokerHealth::unprobed`] states: a claim goes
    /// quiet for one interval and then starts probing.
    pub fn unprobed() -> Self {
        Self {
            probes_disabled: true,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Claim the next probe, if one is due.
    ///
    /// `true` at most once per [`PROBE_INTERVAL`] across every caller, so the
    /// scout loop and the build lane share one subprocess between them and
    /// exactly one of two racing callers gets the [`Transition`] that writes
    /// the `Note`.
    pub fn probe_due(&self, now: DateTime<Utc>) -> bool {
        if self.probes_disabled {
            return false;
        }
        let mut inner = self.inner.lock().expect("runtime health lock");
        // Signed, like `in_force` below: a clock that stepped backwards makes
        // the last probe look like the future, and the answer to that is to
        // probe, never to wait out a window that will not end.
        let due = match inner.probed_at {
            Some(last) => now - last >= interval(),
            None => true,
        };
        if due {
            inner.probed_at = Some(now);
        }
        due
    }

    /// Ask the container runtime whether its services are running.
    pub async fn probe(&self) -> Probe {
        let (program, args) = RUNTIME_PROBE;
        crate::doctor::probe_within(program, &args, PROBE_TIMEOUT).await
    }

    /// Fold one probe in, and say what edge it crossed.
    ///
    /// A zero exit clears. A non-zero exit, or a spawn that failed for a
    /// reason other than the tool being absent, holds. **A missing tool and a
    /// timeout touch nothing** — see the module docs: a host with no
    /// apple/container is not a broken host, and a probe that did not finish
    /// is not an answer.
    pub fn observe(&self, probe: &Probe, now: DateTime<Utc>) -> Transition {
        let failure = match probe {
            Probe::Ran { ok: true, .. } => None,
            Probe::Ran { ok: false, text } => Some(text.clone()),
            Probe::Failed(e) => Some(e.clone()),
            // Absence of evidence never holds — and never releases either.
            Probe::Missing | Probe::TimedOut => return Transition::Unchanged,
        };

        let mut inner = self.inner.lock().expect("runtime health lock");
        let Some(error) = failure else {
            return match inner.down.take() {
                Some(run) if in_force(&run, now) => Transition::Started(run),
                _ => Transition::Unchanged,
            };
        };
        match inner.down.as_mut() {
            Some(run) if in_force(run, now) => {
                run.last = now;
                run.probes += 1;
                run.error = error;
                Transition::Unchanged
            }
            _ => {
                let run = Down {
                    since: now,
                    last: now,
                    probes: 1,
                    error,
                };
                inner.down = Some(run.clone());
                Transition::Stopped(run)
            }
        }
    }

    /// The hold in force right now, if any — the one predicate both gates and
    /// `/status` read.
    pub fn hold(&self, now: DateTime<Utc>) -> Option<Down> {
        let inner = self.inner.lock().expect("runtime health lock");
        inner
            .down
            .as_ref()
            .filter(|run| in_force(run, now))
            .cloned()
    }
}

fn interval() -> chrono::Duration {
    chrono::Duration::from_std(PROBE_INTERVAL).expect("probe interval fits")
}

fn stale_after() -> chrono::Duration {
    chrono::Duration::from_std(STALE_AFTER).expect("staleness window fits")
}

/// Whether a record is recent enough to still be evidence. **Signed**, exactly
/// as the other three are.
fn in_force(run: &Down, now: DateTime<Utc>) -> bool {
    now - run.last <= stale_after()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn running() -> Probe {
        Probe::Ran {
            ok: true,
            text: "apiserver is running".into(),
        }
    }

    /// What the host actually said on 2026-08-19.
    fn stopped() -> Probe {
        Probe::Ran {
            ok: false,
            text: "apiserver is not running and not registered with launchd".into(),
        }
    }

    /// Nothing observed never holds. A server that has not probed yet must
    /// dispatch.
    #[test]
    fn nothing_observed_does_not_hold() {
        assert!(RuntimeHealth::new().hold(at(0)).is_none());
    }

    /// The edges, announced once each — and the report names its own
    /// discharge, because the reader's next question is what to run.
    #[test]
    fn a_stopped_runtime_holds_once_and_a_started_one_releases_it_once() {
        let health = RuntimeHealth::new();

        let Transition::Stopped(run) = health.observe(&stopped(), at(0)) else {
            panic!("the first failed probe is the edge");
        };
        assert_eq!(run.since, at(0));
        let said = run.describe();
        assert!(said.contains("container system start"), "{said}");
        assert!(
            said.contains("not registered with launchd"),
            "it quotes what the runtime said: {said}"
        );
        assert!(health.hold(at(1)).is_some());

        for (i, t) in [30, 60, 90].into_iter().enumerate() {
            assert_eq!(health.observe(&stopped(), at(t)), Transition::Unchanged);
            let held = health.hold(at(t)).expect("still held");
            assert_eq!(held.since, at(0), "`since` is when it stopped");
            assert_eq!(held.last, at(t));
            assert_eq!(held.probes as usize, i + 2);
        }

        let Transition::Started(ended) = health.observe(&running(), at(120)) else {
            panic!("the release is an edge too");
        };
        assert_eq!(ended.probes, 4);
        assert!(health.hold(at(120)).is_none());
        assert_eq!(health.observe(&running(), at(121)), Transition::Unchanged);
    }

    /// **The wedge test.** A host with no `container` on `PATH` is not a
    /// broken host: vm-pool can be built on `SupervisorRuntime`, the test
    /// harnesses are, and a Linux checkout has no apple/container at all. A
    /// hold there would stop a pipeline that works perfectly.
    #[test]
    fn a_missing_container_cli_never_holds() {
        let health = RuntimeHealth::new();
        assert_eq!(
            health.observe(&Probe::Missing, at(0)),
            Transition::Unchanged
        );
        assert!(health.hold(at(0)).is_none());

        // And it is not evidence in the other direction either.
        health.observe(&stopped(), at(10));
        assert_eq!(
            health.observe(&Probe::Missing, at(20)),
            Transition::Unchanged
        );
        assert!(
            health.hold(at(20)).is_some(),
            "an absent tool does not clear a hold; only a zero exit does"
        );
    }

    /// A probe that did not finish is not an answer. `doctor` reads the same
    /// outcome as a `Skip`, and for the same reason: saying we learned
    /// something we did not is the one thing a diagnostic must not do.
    #[test]
    fn a_timed_out_probe_neither_holds_nor_releases() {
        let health = RuntimeHealth::new();
        assert_eq!(
            health.observe(&Probe::TimedOut, at(0)),
            Transition::Unchanged
        );
        assert!(health.hold(at(0)).is_none());

        health.observe(&stopped(), at(10));
        assert_eq!(
            health.observe(&Probe::TimedOut, at(20)),
            Transition::Unchanged
        );
        assert!(health.hold(at(20)).is_some());
    }

    /// A spawn that failed for a reason other than the tool being absent is a
    /// real finding — the tool is there and could not be run.
    #[test]
    fn a_spawn_that_failed_for_another_reason_holds() {
        let health = RuntimeHealth::new();
        assert!(matches!(
            health.observe(&Probe::Failed("permission denied".into()), at(0)),
            Transition::Stopped(_)
        ));
        assert!(health.hold(at(0)).is_some());
    }

    /// The claim is what makes two gates share one subprocess.
    #[test]
    fn one_probe_is_claimed_per_interval_however_many_gates_ask() {
        let health = RuntimeHealth::new();
        assert!(health.probe_due(at(0)), "nothing probed yet");
        assert!(
            !health.probe_due(at(0)),
            "the other gate reads what it wrote"
        );
        assert!(!health.probe_due(at(29)));
        assert!(health.probe_due(at(30)), "due again after the interval");
    }

    /// The test-support record never probes and never holds — structurally,
    /// not for an interval.
    #[test]
    fn an_unprobed_record_never_probes_however_long_it_waits() {
        let health = RuntimeHealth::unprobed();
        assert!(!health.probe_due(at(0)));
        assert!(!health.probe_due(at(86_400)));
        assert!(health.hold(at(86_400)).is_none());
    }

    /// A record nobody refreshes expires. Signed comparison: a clock that
    /// stepped backwards holds rather than releasing.
    #[test]
    fn an_unrefreshed_record_expires_and_a_backwards_clock_does_not_release_it() {
        let health = RuntimeHealth::new();
        health.observe(&stopped(), at(1_000));
        assert!(
            health
                .hold(at(1_000 + STALE_AFTER.as_secs() as i64))
                .is_some()
        );
        assert!(
            health
                .hold(at(1_001 + STALE_AFTER.as_secs() as i64))
                .is_none()
        );
        assert!(health.hold(at(0)).is_some(), "a backwards clock holds");
    }
}
