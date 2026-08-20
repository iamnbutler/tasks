//! Whether the credential broker is answering, and whether dispatch should
//! wait until it is.
//!
//! Every credentialed operation inside a VM is redeemed against the broker —
//! the Anthropic traffic and the **git clone** both — so a broker that stops
//! answering fails every scout and every build at the clone. That is a
//! pre-agent setup failure, which the strike rule charges *deliberately* (a
//! clone against a base branch that is gone fails identically forever, and
//! waiving it would retry forever with nothing to stop it). So an outage of one
//! minute does not delay work, it destroys it: on 2026-08-18 19:35–19:36 UTC,
//! #996 burned two attempts in 12 seconds and #982 burned all three in 27, and
//! both were `rejected` — a terminal state — for a fault neither task had
//! anything to do with. Ten more queued tasks were seconds behind and survive
//! only because a human paused dispatch by hand (#1006).
//!
//! #939 settled where the fix goes: **in dispatch, not in classification**.
//! This is the fourth standing hold, beside [`crate::github_health`],
//! [`crate::updates`] and [`crate::pool_health`], at the same gates and in
//! memory for the same reason — it is a fact about a listener with a timestamp
//! on it.
//!
//! Nothing that already exists covers it. [`crate::github_health`] is written
//! from calls the *poller* makes, and the poller talks to `api.github.com`
//! directly with the server's own credential: GitHub was perfectly healthy
//! throughout that outage. [`crate::pool_health`] has the wrong subject — a
//! pool with free slots and a dead broker allocates happily and then dies at
//! the clone.
//!
//! ## The evidence is a probe, and it has to be
//!
//! This is the one thing genuinely harder here than for
//! [`crate::github_health`], and it is worth stating rather than assuming. The
//! broker's own successful request-serving would be the honest passive signal,
//! but **during an outage there are no requests to observe**: the runs that
//! would generate them are exactly the runs that are failing. A passive record
//! would therefore be blank at precisely the moment it is needed, and the only
//! thing that clears it — a served request — is the thing the outage prevents.
//!
//! So the record is written from an active probe, like [`crate::pool_health`]'s
//! and unlike [`crate::github_health`]'s, and it reuses
//! [`crate::doctor::probe_broker_within`] rather than growing a second opinion
//! about what a healthy broker looks like. Two properties of that probe are
//! load-bearing here and both are already argued in `doctor`:
//!
//! - **It goes to the advertised address, never loopback.** During the
//!   firewall outage that produced the check, loopback answered a correct 401
//!   while the bridge gateway accepted the connection and returned zero bytes.
//!   A `127.0.0.1` probe reads as a pass at exactly the moment the thing is
//!   broken.
//! - **An unauthenticated 401 is the success condition.** Every broker route
//!   demands a lease before it does anything else, so an unauthenticated
//!   request is a complete, side-effect-free question.
//!
//! It also settles the issue's second open question. The clone failure is
//! reported by the supervisor, inside the VM, as prose (`clone: … Empty reply
//! from server`), and deciding a hold off message text is what `FailureClass`
//! and `GhError::is_unavailable` forbid. A host-side probe needs no protocol
//! change and no image rebuild, and it observes the fault *before* a VM is
//! spent on it — which the in-VM signal, arriving after the allocation and the
//! teardown, structurally cannot.
//!
//! ## Which answers hold
//!
//! The three rules that keep a hold from becoming a silent stall are the same
//! three, and the first one bites harder here than anywhere else in the tree:
//!
//! **Absence of evidence never holds** — and the sharpest case is
//! [`BrokerProbe::Unreachable`]. apple/container's bridge gateway *does not
//! exist until the first container has started*, so on a cold machine the
//! advertised address is unreachable as a matter of course. Holding on it
//! would prevent the container that creates the gateway, so the gateway would
//! never appear and the hold would never clear. It is a wedge only the gate
//! itself keeps closed — the same shape the
//! update watch refuses for a pre-boot image observation — so `Unreachable`
//! touches nothing at all. `doctor` reaches the same verdict from the same
//! premise and calls it a `Skip`.
//!
//! **Only a fresh success clears one.** A 401 clears; nothing else does, and
//! in particular an unreachable address does not release a hold that a silent
//! listener set.
//!
//! **A hold nobody refreshes expires**, at [`STALE_AFTER`], so `/status`
//! cannot report one from a dispatcher that stopped asking.
//!
//! And one answer deliberately does *not* hold: [`BrokerProbe::SpokeHttp`] —
//! something is listening and it is speaking HTTP. Whether it is the broker is
//! a question a probe cannot settle, and this is the same line
//! `GhError::is_unavailable` draws when it excludes every `4xx`: a service
//! that *answers* is not an outage, and a hold set on one would have no
//! clearing signal of its own. `doctor` warns about it and does not fail, for
//! the same reason.
//!
//! What holding buys is both halves of the cost. The strike is not charged
//! because the run never starts — and the VM is not allocated, held and torn
//! down either, which the five attempts on that night each did.

use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use tasks_api::http::BrokerHold;

use crate::doctor::BrokerProbe;

/// How often the record is refreshed. One TCP round trip to the bridge
/// gateway, claimed by whichever gate asks first.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// How long the probe waits before calling the broker silent.
///
/// Deliberately shorter than `doctor`'s ten seconds: this one runs on the
/// dispatch path, where a gate awaiting it stalls the tick, and a broker that
/// has not answered in three seconds is one a clone is not going to reach
/// either. `doctor` keeps the longer budget because a human ran it and wants
/// the most patient reading available.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a record stays evidence without a refresh.
///
/// The gates refresh it as they read it, so in the steady state nothing
/// reaches this; it is the backstop for a dispatcher that stopped asking.
pub const STALE_AFTER: Duration = Duration::from_secs(180);

/// The invariant is a *relationship* between the two, not two knobs: the
/// window has to outlast several probes or a live gate would expire its own
/// record between reads.
const _: () = assert!(STALE_AFTER.as_secs() >= 4 * PROBE_INTERVAL.as_secs());

/// A run of probes in which the broker did not answer, not yet ended by one
/// where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    /// The first failed probe in this run. **Not** moved by later ones: a
    /// human asking why nothing is dispatching wants "down since 19:35".
    pub since: DateTime<Utc>,
    /// The most recent failed probe, which is what keeps the record from going
    /// stale under a live gate.
    pub last: DateTime<Utc>,
    /// How many probes have failed since `since`.
    pub probes: u32,
    /// The advertised address probed, so the report names the thing to check
    /// rather than the concept.
    pub address: String,
    /// The most recent failure, rendered — prose for a reader.
    pub error: String,
}

impl Outage {
    /// The sentence the event log and `/status` get.
    pub fn describe(&self) -> String {
        format!(
            "the credential broker at {} is not answering since {} ({} probe(s); latest: \
             {}). Scout and build dispatch waits for it: every clone inside a VM is \
             redeemed there, so work dispatched now would die at the clone and be charged \
             for it. Queued work stays queued and nothing is charged an attempt",
            self.address,
            self.since.to_rfc3339(),
            self.probes,
            self.error,
        )
    }

    /// The wire shape `/status` reports.
    pub fn to_hold(&self) -> BrokerHold {
        BrokerHold {
            since: self.since,
            last_seen: self.last,
            probes: self.probes,
            address: self.address.clone(),
            error: self.error.clone(),
        }
    }
}

/// What one probe did to the record — the edge, so a caller can announce it
/// exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing an announcement would be about: an answering broker while it
    /// was answering, a further failed probe inside a hold already in force,
    /// or a probe that observed nothing either way.
    Unchanged,
    /// A hold just went on.
    Down(Outage),
    /// A hold that was in force just came off.
    Back(Outage),
}

/// Whether the broker is answering, as last probed.
///
/// One record, shared by both dispatchers and `/status`, so the three cannot
/// disagree about whether a hold is in force.
#[derive(Debug)]
pub struct BrokerHealth {
    /// The advertised host — `TASKS_BROKER_ADVERTISE`, the address VMs use.
    /// Held here rather than passed per call so no caller can probe loopback
    /// by accident, which is the one way this check reads as a pass while the
    /// thing is broken.
    host: String,
    port: u16,
    /// Set only by [`BrokerHealth::unprobed`]. A record that never probes
    /// never observes, and one that never observes never holds — which is the
    /// first of the three rules, read as a construction rather than as a
    /// state.
    probes_disabled: bool,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    outage: Option<Outage>,
    /// When a probe was last *claimed* — not when it answered. Claiming is
    /// what makes two gates share one round trip.
    probed_at: Option<DateTime<Utc>>,
}

impl BrokerHealth {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            probes_disabled: false,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The address the probe goes to, as the report names it.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Claim the next probe, if one is due.
    ///
    /// `true` at most once per [`PROBE_INTERVAL`] across every caller: the
    /// claim is taken under the lock, so of two gates asking in the same tick
    /// one probes and the other reads what it wrote.
    pub fn probe_due(&self, now: DateTime<Utc>) -> bool {
        if self.probes_disabled {
            return false;
        }
        let mut inner = self.inner.lock().expect("broker health lock");
        let due = match inner.probed_at {
            // Signed, like `in_force` below: a clock that stepped backwards
            // makes the last probe look like the future, and the answer to
            // that is to probe, never to wait out a window that will not end.
            Some(last) => now - last >= interval(),
            None => true,
        };
        if due {
            inner.probed_at = Some(now);
        }
        due
    }

    /// Ask the advertised address whether it is answering.
    ///
    /// Reuses `doctor`'s probe rather than growing a second opinion about what
    /// a healthy broker looks like — the same rule that has `doctor` read
    /// `ImageFreshness::needs_rebuild` instead of judging freshness twice.
    pub async fn probe(&self) -> BrokerProbe {
        crate::doctor::probe_broker_within(&self.host, self.port, PROBE_TIMEOUT).await
    }

    /// Fold one probe in, and say what edge it crossed.
    ///
    /// See the module docs for why each answer reads the way it does. In
    /// short: a 401 clears, a listener that spoke HTTP is not an outage, a
    /// silent or refused listener holds, and an address that cannot be reached
    /// at all touches nothing — because on a cold machine that is the ordinary
    /// answer, and holding on it is a wedge only the gate keeps closed.
    pub fn observe(&self, probe: &BrokerProbe, now: DateTime<Utc>) -> Transition {
        let failure = match probe {
            BrokerProbe::DemandedLease => None,
            // Something answered. Not an outage, by the same line
            // `GhError::is_unavailable` draws at `4xx`.
            BrokerProbe::SpokeHttp(_) => None,
            BrokerProbe::Silent(what) => {
                Some(format!("it accepted the connection and then {what}"))
            }
            BrokerProbe::Refused(e) => Some(format!("nothing is listening there ({e})")),
            // Absence of evidence never holds — and never releases either.
            BrokerProbe::Unreachable(_) => return Transition::Unchanged,
        };

        let mut inner = self.inner.lock().expect("broker health lock");
        let Some(error) = failure else {
            return match inner.outage.take() {
                Some(run) if in_force(&run, now) => Transition::Back(run),
                _ => Transition::Unchanged,
            };
        };
        match inner.outage.as_mut() {
            Some(run) if in_force(run, now) => {
                run.last = now;
                run.probes += 1;
                run.error = error;
                Transition::Unchanged
            }
            // Nothing held, or what was held had already expired: a fresh edge
            // either way, and `since` starts here.
            _ => {
                let run = Outage {
                    since: now,
                    last: now,
                    probes: 1,
                    address: format!("{}:{}", self.host, self.port),
                    error,
                };
                inner.outage = Some(run.clone());
                Transition::Down(run)
            }
        }
    }

    /// A record that never probes and therefore never holds.
    ///
    /// For tests of the *other* gates, which have no broker to answer them.
    /// Not a matter of taste: an ordinary record pointed at an address with
    /// nothing behind it reads [`BrokerProbe::Refused`] and holds, which is
    /// the correct production answer and the wrong one in a test about
    /// something else — so every dispatch test in the tree would start
    /// failing for this module's reasons rather than its own.
    ///
    /// **Structural rather than a pre-claimed probe**: claiming one would go
    /// quiet for [`PROBE_INTERVAL`] and then start probing, so a test that
    /// happened to run longer than fifteen seconds would fail intermittently
    /// and for a reason with nothing to do with it.
    ///
    /// `pub` on the [`crate::secrets::Secrets::for_tests`] precedent —
    /// integration tests live outside this crate, so `#[cfg(test)]` does not
    /// reach them.
    pub fn unprobed() -> Self {
        Self {
            probes_disabled: true,
            ..Self::new("127.0.0.1", 0)
        }
    }

    /// The hold in force right now, if any — the one predicate both gates and
    /// `/status` read.
    pub fn hold(&self, now: DateTime<Utc>) -> Option<Outage> {
        let inner = self.inner.lock().expect("broker health lock");
        inner
            .outage
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

/// Whether a record is recent enough to still be evidence.
///
/// **Signed**, exactly as [`crate::github_health`]'s and
/// [`crate::pool_health`]'s are: a clock that stepped backwards makes the
/// record look like it came from the future, which is the one thing it
/// certainly is not, and an absolute age would read the step as "far too old"
/// and release a hold set moments ago.
fn in_force(run: &Outage, now: DateTime<Utc>) -> bool {
    now - run.last <= stale_after()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn health() -> BrokerHealth {
        BrokerHealth::new("192.168.64.1", 4801)
    }

    /// Nothing observed never holds — the rule that keeps a hold from being a
    /// wedge. A server that has not probed yet must dispatch.
    #[test]
    fn nothing_observed_does_not_hold() {
        assert!(health().hold(at(0)).is_none());
    }

    /// The edges, announced once each: the first silent probe holds, later
    /// ones are silent, and the first 401 releases it.
    #[test]
    fn a_silent_broker_holds_once_and_a_lease_demand_releases_it_once() {
        let health = health();
        let silent = BrokerProbe::Silent("it returned no bytes at all".into());

        let Transition::Down(run) = health.observe(&silent, at(0)) else {
            panic!("the first silent probe is the edge");
        };
        assert_eq!(run.since, at(0));
        assert_eq!(run.address, "192.168.64.1:4801");
        assert!(health.hold(at(1)).is_some());

        for (i, t) in [15, 30, 45].into_iter().enumerate() {
            assert_eq!(health.observe(&silent, at(t)), Transition::Unchanged);
            let held = health.hold(at(t)).expect("still held");
            assert_eq!(held.since, at(0), "`since` is when it went down");
            assert_eq!(held.last, at(t), "`last` is what keeps it alive");
            assert_eq!(held.probes as usize, i + 2);
        }

        let Transition::Back(ended) = health.observe(&BrokerProbe::DemandedLease, at(60)) else {
            panic!("the release is an edge too");
        };
        assert_eq!(ended.probes, 4);
        assert!(health.hold(at(60)).is_none());
        // And a second healthy probe says nothing.
        assert_eq!(
            health.observe(&BrokerProbe::DemandedLease, at(61)),
            Transition::Unchanged
        );
    }

    /// The observed failure: the connection is accepted and nothing comes
    /// back. That is what `clone: … Empty reply from server` is, seen from the
    /// host, and the whole reason this module exists.
    #[test]
    fn an_empty_reply_is_the_outage_this_exists_for() {
        let health = health();
        let Transition::Down(run) = health.observe(
            &BrokerProbe::Silent("it accepted the connection and never answered".into()),
            at(0),
        ) else {
            panic!("a silent broker holds");
        };
        let said = run.describe();
        assert!(said.contains("192.168.64.1:4801"), "{said}");
        assert!(
            said.contains("die at the clone"),
            "the report names what dispatching anyway would cost: {said}"
        );
    }

    /// A refused connection holds too: nothing is listening, so every clone
    /// fails. It is a different sentence from silence and the same verdict.
    #[test]
    fn a_refused_connection_holds() {
        let health = health();
        assert!(matches!(
            health.observe(&BrokerProbe::Refused("connection refused".into()), at(0)),
            Transition::Down(_)
        ));
        assert!(health.hold(at(0)).is_some());
    }

    /// **The wedge test.** apple/container's bridge gateway does not exist
    /// until the first container has started, so on a cold machine the
    /// advertised address is unreachable as a matter of course. A hold on that
    /// would prevent the container that creates the gateway — a gate only the
    /// gate itself keeps closed — so an unreachable address must dispatch.
    #[test]
    fn an_unreachable_gateway_never_holds_because_a_container_is_what_creates_it() {
        let health = health();
        let cold = BrokerProbe::Unreachable("no route to host".into());

        assert_eq!(health.observe(&cold, at(0)), Transition::Unchanged);
        assert!(
            health.hold(at(0)).is_none(),
            "a cold machine has not been shown to be broken"
        );
        // And it is not evidence in the other direction either: it must not
        // release a hold a silent listener set.
        health.observe(&BrokerProbe::Silent("nothing".into()), at(10));
        assert_eq!(health.observe(&cold, at(20)), Transition::Unchanged);
        assert!(
            health.hold(at(20)).is_some(),
            "an unreachable address does not clear a hold; only a 401 does"
        );
    }

    /// A listener that speaks HTTP is answering, whatever it answered. That is
    /// the line `GhError::is_unavailable` draws at `4xx`, and a hold set on it
    /// would have no clearing signal of its own — the thing answering would go
    /// on answering.
    #[test]
    fn a_listener_that_spoke_http_is_not_an_outage() {
        let health = health();
        assert_eq!(
            health.observe(&BrokerProbe::SpokeHttp(502), at(0)),
            Transition::Unchanged
        );
        assert!(health.hold(at(0)).is_none());
        // And it clears a hold, because something is serving that port again.
        health.observe(&BrokerProbe::Silent("nothing".into()), at(10));
        assert!(matches!(
            health.observe(&BrokerProbe::SpokeHttp(200), at(20)),
            Transition::Back(_)
        ));
    }

    /// The claim is what makes two gates share one round trip — and what makes
    /// exactly one of two racing callers get the transition that writes the
    /// `Note`.
    #[test]
    fn one_probe_is_claimed_per_interval_however_many_gates_ask() {
        let health = health();
        assert!(health.probe_due(at(0)), "nothing probed yet");
        assert!(
            !health.probe_due(at(0)),
            "the other gate reads what it wrote"
        );
        assert!(!health.probe_due(at(14)));
        assert!(health.probe_due(at(15)), "due again after the interval");
    }

    /// The test-support record never probes and never holds — structurally,
    /// not for an interval. A gate reading it a day later still dispatches.
    #[test]
    fn an_unprobed_record_never_probes_however_long_it_waits() {
        let health = BrokerHealth::unprobed();
        assert!(!health.probe_due(at(0)));
        assert!(!health.probe_due(at(86_400)));
        assert!(health.hold(at(86_400)).is_none());
    }

    /// A record nobody refreshes expires, so `/status` cannot report a hold
    /// from a dispatcher that stopped asking. Signed comparison: a clock that
    /// stepped backwards holds rather than releasing.
    #[test]
    fn an_unrefreshed_record_expires_and_a_backwards_clock_does_not_release_it() {
        let health = health();
        health.observe(&BrokerProbe::Silent("nothing".into()), at(1_000));
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
