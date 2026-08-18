//! Whether an upgrade is half-applied, and whether new containers should wait
//! for it.
//!
//! An upgrade here is never atomic: the server binary, the VM images and the
//! running process are updated by three different acts (`cargo build`,
//! `make images`, `tasks reload`), and the gaps between them are where work
//! gets dispatched into the stale half. #888's fix sat on `main` for ten hours
//! while a scout died of the exact bug inside an older image; a rebuilt server
//! binary sits on disk dispatching nothing newer than the process that ignores
//! it. The observation half of that problem is solved — [`crate::images`]
//! records what runs report, `/status` renders it — but observing a gap and
//! *walking into it* were still the same behaviour. This module is the other
//! half: while an update is pending, new containers wait.
//!
//! Two skews are observable from inside this process, and they are the whole
//! definition of "pending":
//!
//! - **A newer server binary on disk.** The file at [`std::env::current_exe`]
//!   with an mtime after this process booted is a build someone made and has
//!   not swapped in; `make restart` discharges it. A stat per tick, no cache.
//! - **A stale VM image, observed under this server.** An image identity whose
//!   `ImageFreshness::needs_rebuild()` holds against this server's stamp, from
//!   an observation made **since this process booted**; `make images` then
//!   `make restart` discharges it. Both halves of "observed" are load-bearing.
//!   An image nothing has run in reports nothing, and **absence of evidence
//!   never holds** — the run that would observe the image is the run the hold
//!   would prevent (same rule as [`crate::github_health`]'s first). And an
//!   observation from *before* this boot is stale data, not evidence: every
//!   image reads `behind` the moment a newer server starts, the record only
//!   moves when a run moves it, and a rebuild does not touch it — so holding
//!   on old observations is a gate only the gate itself keeps closed. The
//!   cost of the fresh-only rule is honest and bounded: after an upgrade, one
//!   run may start in a genuinely stale image; it reports the fact and closes
//!   the gate behind itself.
//!
//! The hold gates exactly what the mode gates: *new* dispatch — the scout
//! top-up and the build claim, the only two places a container starts. Work in
//! flight runs to completion, queued work stays queued, nothing is charged an
//! attempt, and `/status` answers with the reasons and their discharges. The
//! transition is announced once per edge by whoever computes it first — in the
//! log, and as an [`crate::events::EventPayload::Note`] on the event feed, the
//! same shape the GitHub hold uses. `/status` is the standing answer for
//! whoever arrives after the edge has scrolled past.
//!
//! `TASKS_UPDATE_HOLD=off` turns the gate off (the observation and the
//! `/status` report remain); anything that is neither `on` nor `off` refuses
//! to boot, like every other switch here.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::store::Store;
use tasks_api::http::UpdatePending;

/// The watch one server process keeps over its own staleness.
///
/// Constructed once at boot — the boot instant is the reference the binary
/// probe compares against, so constructing it later would hide a build that
/// happened in between.
pub struct UpdateWatch {
    /// When this process started; an executable newer than this is pending.
    boot: SystemTime,
    /// [`Self::boot`] again, on the clock image observations are stamped
    /// with. Captured together so the two probes cannot disagree about when
    /// "since boot" starts.
    booted_at: DateTime<Utc>,
    /// Where this process was started from. `None` when the OS cannot say —
    /// which reads as "no binary skew observable", never as a hold.
    exe: Option<PathBuf>,
    /// `TASKS_UPDATE_HOLD`. Off, the probes still run and `/status` still
    /// reports; only the dispatchers stop listening.
    enabled: bool,
    /// The last announced reasons, so the transition logs exactly once no
    /// matter how many loops consult the watch.
    announced: Mutex<Option<Vec<String>>>,
}

impl UpdateWatch {
    pub fn at_boot(enabled: bool) -> Self {
        Self {
            boot: SystemTime::now(),
            booted_at: Utc::now(),
            exe: std::env::current_exe().ok(),
            enabled,
            announced: Mutex::new(None),
        }
    }

    /// What is pending, whether or not the gate is on — the `/status` answer.
    ///
    /// Announces transitions as a side effect: a `warn!` when a hold goes on
    /// or changes shape, an `info!` when it comes off. Deduplicated across
    /// every caller through [`Self::announced`], so the dispatchers and the
    /// status handler can all ask without turning the log into a metronome.
    pub async fn pending(&self, store: &Store) -> Option<UpdatePending> {
        let mut reasons = Vec::new();
        if let Some(reason) = self.binary_pending() {
            reasons.push(reason);
        }
        match store.image_builds(crate::version::VERSION).await {
            Ok(images) => {
                for image in images {
                    // Only an observation made *under this server* can hold.
                    // An older one is stale data about an image that may have
                    // been rebuilt since — every image reads `behind` the
                    // moment a newer server boots, and nothing but a run
                    // refreshes the record, so holding on it would be a gate
                    // only the gate itself keeps closed. This is the no-wedge
                    // rule made concrete: after an upgrade the first run
                    // dispatches, observes what the image really is now, and
                    // closes the gate behind itself only if it truly is stale.
                    if image.freshness.needs_rebuild() && image.observed_at >= self.booted_at {
                        reasons.push(format!(
                            "VM image {} is {} ({} vs this server's {}); run `make images`, \
                             then `make restart`",
                            image.image,
                            image.freshness.as_str(),
                            image.version.as_deref().unwrap_or("unstamped"),
                            crate::version::VERSION,
                        ));
                    }
                }
            }
            // An unreadable store must not decide anything here: this is an
            // observation about the infrastructure, and the dispatcher that
            // asked has its own, louder ways to fail on a broken store.
            Err(err) => warn!(error = %err, "could not read image identities for the update watch"),
        }

        self.announce(store, &reasons).await;
        (!reasons.is_empty()).then_some(UpdatePending {
            reasons,
            enforced: self.enabled,
        })
    }

    /// Whether the two dispatchers should wait. The one place
    /// `TASKS_UPDATE_HOLD` is consulted, so the gate and the report cannot
    /// disagree about what is pending — only about whether it binds.
    pub async fn hold(&self, store: &Store) -> bool {
        self.enabled && self.pending(store).await.is_some()
    }

    /// A newer build of this very binary, sitting at the path we were started
    /// from.
    fn binary_pending(&self) -> Option<String> {
        let exe = self.exe.as_ref()?;
        let mtime = std::fs::metadata(exe).ok()?.modified().ok()?;
        binary_is_newer(mtime, self.boot).then(|| {
            format!(
                "a newer server binary was built at {} after this process started; \
                 run `make restart` to swap it in",
                exe.display()
            )
        })
    }

    /// Say once, per edge, that the hold went on or came off — in the log and
    /// on the event feed.
    ///
    /// The feed half is what a reader who was not watching the terminal gets:
    /// a `Note` (source [`UPDATE_WATCH`]), exactly the shape
    /// [`crate::run::GitHubWatch::observe`] uses for the other hold, and for
    /// the same reasons. A `Note` is not `nudge_worthy`, so it costs no
    /// orchestrator turn, and it is deliberately **not** an obligation — the
    /// orchestrator holds a curl-only token in a VM-less workdir and could no
    /// more run `make images` than it could fix GitHub, which is the argument
    /// that keeps `ObligationKind::StaleImage` from existing.
    ///
    /// The guard is taken to *claim* the edge and released before the await.
    /// Both halves matter: holding a [`std::sync::Mutex`] across an await
    /// would make this future non-`Send` and the spawned dispatch loops would
    /// not compile, and claiming under the lock is what makes exactly one of
    /// two racing callers write the `Note`.
    async fn announce(&self, store: &Store, reasons: &[String]) {
        let message = {
            let mut announced = self.announced.lock().expect("update watch poisoned");
            let current = (!reasons.is_empty()).then(|| reasons.to_vec());
            if *announced == current {
                return;
            }
            *announced = current.clone();
            match &current {
                Some(reasons) => {
                    let effect = match self.enabled {
                        true => {
                            "new scouts and builds wait until it is applied \
                                 (queued work stays queued, nothing is charged)"
                        }
                        false => "TASKS_UPDATE_HOLD=off: reported, not enforced",
                    };
                    warn!(
                        enforced = self.enabled,
                        reasons = reasons.join("; "),
                        "an update is pending ({effect})"
                    );
                    format!("an update is pending ({effect}): {}", reasons.join("; "))
                }
                None => {
                    info!("the pending update was applied; dispatch resumes");
                    "the pending update was applied; dispatch resumes".to_string()
                }
            }
        };
        if let Err(err) = store
            .append_event(crate::events::EventPayload::Note {
                source: UPDATE_WATCH.into(),
                message,
            })
            .await
        {
            warn!(error = %err, "could not record the update hold on the feed");
        }
    }
}

/// Source tag on the notes this module appends, so a reader can tell the
/// update hold's edges from the GitHub hold's.
pub const UPDATE_WATCH: &str = "update-watch";

/// The pure half of the binary probe. `>` and not `>=`: an exe written in the
/// same instant the process booted is the build that booted.
fn binary_is_newer(exe_mtime: SystemTime, boot: SystemTime) -> bool {
    exe_mtime > boot
}

/// Parse `TASKS_UPDATE_HOLD`: absent or `on` gates, `off` reports without
/// gating, anything else refuses to boot — a switch that quietly defaulted on
/// a typo would fail in exactly the direction it exists to prevent. Pure, so
/// it is testable without racing every other test through `set_var`; the env
/// read lives with the rest of the config in [`crate::run::Config`].
pub fn parse_enabled(value: Option<&str>) -> Result<bool, String> {
    match value {
        None => Ok(true),
        Some(value) => match value.trim() {
            "on" | "" => Ok(true),
            "off" => Ok(false),
            other => Err(format!(
                "TASKS_UPDATE_HOLD must be `on` or `off`, got `{other}`"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::protocol::SupervisorBuild;
    use tasks_api::version::ImageRole;

    #[test]
    fn a_binary_built_after_boot_is_pending_and_one_from_before_is_not() {
        let boot = SystemTime::now();
        assert!(!binary_is_newer(boot - Duration::from_secs(60), boot));
        assert!(!binary_is_newer(boot, boot), "the build that booted");
        assert!(binary_is_newer(boot + Duration::from_secs(1), boot));
    }

    /// The whole point of observed-only: an empty record holds nothing, a
    /// stale observation holds, and a current one releases.
    #[tokio::test]
    async fn images_hold_only_on_an_observed_stale_identity() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let watch = UpdateWatch::at_boot(true);

        // Nothing observed yet: no reasons, no hold. The run that would
        // observe the image must be allowed to start.
        assert!(watch.pending(&store).await.is_none());
        assert!(!watch.hold(&store).await);

        // A run reports an identity older than this server's stamp.
        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: "0.1.1".into(),
                    commit: "0000000".into(),
                }),
                "sess_test",
            )
            .await
            .unwrap();
        let pending = watch.pending(&store).await.expect("a stale image holds");
        assert!(pending.enforced);
        assert_eq!(pending.reasons.len(), 1);
        assert!(
            pending.reasons[0].contains("agent:v1") && pending.reasons[0].contains("make images"),
            "{:?}",
            pending.reasons
        );
        assert!(watch.hold(&store).await);

        // The rebuilt image is observed: released.
        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: crate::version::VERSION.into(),
                    commit: "0000000".into(),
                }),
                "sess_test2",
            )
            .await
            .unwrap();
        assert!(watch.pending(&store).await.is_none());
        assert!(!watch.hold(&store).await);
    }

    /// The no-wedge half of "observed": an observation from before this boot
    /// is stale data about an image that may have been rebuilt since, and it
    /// must not hold — the record only moves when a run moves it, and the
    /// hold is what would stop every run.
    #[tokio::test]
    async fn an_observation_from_before_this_boot_never_holds() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: "0.1.1".into(),
                    commit: "0000000".into(),
                }),
                "sess_old",
            )
            .await
            .unwrap();
        // A later boot: the recorded staleness predates it.
        std::thread::sleep(Duration::from_millis(10));
        let watch = UpdateWatch::at_boot(true);
        assert!(
            watch.pending(&store).await.is_none(),
            "pre-boot observations are stale data, not a hold"
        );
        assert!(!watch.hold(&store).await);
    }

    /// `off` is report-without-gate: the reasons still surface (so `/status`
    /// stays honest) and the dispatchers do not wait.
    #[tokio::test]
    async fn switched_off_the_watch_reports_and_does_not_hold() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let watch = UpdateWatch::at_boot(false);
        store
            .record_image_build("agent:v1", ImageRole::Scout, None, "sess_unstamped")
            .await
            .unwrap();
        let pending = watch.pending(&store).await.expect("still reported");
        assert!(!pending.enforced);
        assert!(!watch.hold(&store).await, "reported, never enforced");
    }

    /// The edge reaches the feed, not only the log. `/status`, `tasks status`
    /// and the Server window are the standing answer; this is what a reader
    /// who was not watching gets — and it is a `Note`, so it costs no
    /// orchestrator turn.
    #[tokio::test]
    async fn each_edge_of_the_hold_lands_on_the_feed_exactly_once() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let watch = UpdateWatch::at_boot(true);

        // Nothing pending: nothing to say.
        watch.pending(&store).await;
        assert_eq!(notes(&store).await.len(), 0);

        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: "0.1.1".into(),
                    commit: "0000000".into(),
                }),
                "sess_stale",
            )
            .await
            .unwrap();
        // Every loop consults the watch; only the edge is announced.
        watch.pending(&store).await;
        watch.hold(&store).await;
        watch.pending(&store).await;
        let held = notes(&store).await;
        assert_eq!(held.len(), 1, "once per edge, not once per tick: {held:?}");
        assert!(held[0].contains("an update is pending"), "{}", held[0]);
        assert!(held[0].contains("agent:v1"), "{}", held[0]);
        assert!(held[0].contains("make images"), "{}", held[0]);

        // …and the release is its own edge.
        store
            .record_image_build(
                "agent:v1",
                ImageRole::Scout,
                Some(&SupervisorBuild {
                    version: crate::version::VERSION.into(),
                    commit: "0000000".into(),
                }),
                "sess_fresh",
            )
            .await
            .unwrap();
        watch.pending(&store).await;
        watch.pending(&store).await;
        let both = notes(&store).await;
        assert_eq!(both.len(), 2, "{both:?}");
        assert!(both[1].contains("dispatch resumes"), "{}", both[1]);
    }

    /// Reported-not-enforced is a different sentence, and the feed has to
    /// carry the difference: a hold nobody is honouring must not read as one
    /// that is.
    #[tokio::test]
    async fn a_reported_hold_says_it_is_not_enforced() {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let watch = UpdateWatch::at_boot(false);
        store
            .record_image_build("agent:v1", ImageRole::Scout, None, "sess_unstamped")
            .await
            .unwrap();
        watch.pending(&store).await;
        let notes = notes(&store).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("TASKS_UPDATE_HOLD=off"), "{}", notes[0]);
    }

    /// The messages of every `update-watch` note on the feed, in order.
    async fn notes(store: &Store) -> Vec<String> {
        store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                crate::events::EventPayload::Note { source, message } if source == UPDATE_WATCH => {
                    Some(message)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_switch_parses_on_off_and_refuses_the_rest() {
        assert_eq!(parse_enabled(None), Ok(true));
        assert_eq!(parse_enabled(Some("on")), Ok(true));
        assert_eq!(parse_enabled(Some("off")), Ok(false));
        assert_eq!(parse_enabled(Some(" off ")), Ok(false));
        assert!(parse_enabled(Some("maybe")).is_err());
    }
}
