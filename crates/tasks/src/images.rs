//! What the VM images are running, observed from the runs inside them.
//!
//! The pipeline installs fixes to its own supervisors only when a human runs
//! `make images`, and nothing anywhere noticed that nobody had — #888's fix
//! for a dropped API connection sat on `main` for ten hours while that exact
//! failure killed a 52-minute scout inside an image built before it, and the
//! old supervisor, having no idea it was old, reported `SPEC.md not found` and
//! charged a dispatch strike.
//!
//! Nothing polls an image: a VM exists only while a run is inside it, so the
//! `Started` event of each protocol is the only moment there is to ask. This
//! module is where that answer lands. The verdict is never stored — see
//! [`crate::store::Store::image_builds`] — and the rebuild itself stays
//! manual, because nothing inside the pipeline can reach the cross toolchain,
//! the `container` CLI or the checkout a rebuild needs. The failure mode was
//! never that the rebuild was manual; it was that the gap was invisible.

use tracing::{info, warn};

use crate::events::EventPayload;
use crate::protocol::SupervisorBuild;
use crate::store::Store;
use tasks_api::version::ImageRole;

/// Record what an image reported on `Started`, and say so where it will be
/// read.
///
/// Two different noises, deliberately:
///
/// - A `warn!` on **every** run that starts in a stale image. That reading is
///   wanted next to *that run's* own output, at the moment a later failure in
///   the same log would be explained by it — which is exactly what #884 got
///   wrong when it read an infrastructure death as a verdict on the work.
/// - An [`EventPayload::Note`] only when the recorded identity **changes**. A
///   stale image stays stale, so announcing it per dispatch would be the noise
///   the standing `/status` line exists to replace. A `Note` and not a
///   nudge-worthy payload, so it costs no orchestrator turn.
///
/// Failures here are logged and swallowed. This is an observation about the
/// infrastructure; it must never be the reason a run fails.
pub async fn observe(
    store: &Store,
    image: &str,
    role: ImageRole,
    build: Option<&SupervisorBuild>,
    run_id: &str,
) {
    let reference = crate::version::VERSION;
    let freshness =
        tasks_api::version::ImageFreshness::judge(build.map(|b| b.version.as_str()), reference);

    if freshness.needs_rebuild() {
        warn!(
            %image,
            role = role.as_str(),
            %run_id,
            image_version = build.map(|b| b.version.as_str()).unwrap_or("none reported"),
            server_version = reference,
            verdict = freshness.as_str(),
            "this run started in a stale VM image; run `make images` on the host. \
             A failure below may be a fixed bug still shipping in this image rather \
             than anything about the work"
        );
    } else {
        info!(
            %image,
            role = role.as_str(),
            image_version = build.map(|b| b.version.as_str()).unwrap_or("none reported"),
            verdict = freshness.as_str(),
            "image identity observed"
        );
    }

    let changed = match store.record_image_build(image, role, build, run_id).await {
        Ok(changed) => changed,
        Err(err) => {
            warn!(%image, error = %err, "recording the image identity failed");
            return;
        }
    };
    if !changed {
        return;
    }
    let message = match build {
        Some(build) => format!(
            "{image} ({}) is now running supervisor {} ({}); this server is {reference}, so the \
             image reads as {}",
            role.as_str(),
            build.version,
            build.commit,
            freshness.as_str(),
        ),
        None => format!(
            "{image} ({}) reports no build identity, so it predates supervisor stamping and is \
             older than anything that could report one. Run `make images` on the host",
            role.as_str(),
        ),
    };
    if let Err(err) = store
        .append_event(EventPayload::Note {
            source: "images".into(),
            message,
        })
        .await
    {
        warn!(%image, error = %err, "appending the image identity note failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The change test is what keeps this off every dispatch: an image whose
    /// identity has not moved is recorded again and produces no second note.
    #[tokio::test]
    async fn only_a_changed_identity_is_announced() {
        let store = Store::open_in_memory().await.unwrap();
        let build = SupervisorBuild {
            version: "0.1.100".into(),
            commit: "abc1234".into(),
        };

        observe(&store, "agent:v1", ImageRole::Scout, Some(&build), "sess_1").await;
        observe(&store, "agent:v1", ImageRole::Scout, Some(&build), "sess_2").await;

        let notes = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| matches!(e.payload, EventPayload::Note { .. }))
            .count();
        assert_eq!(notes, 1, "a stale image stays stale; say it once");

        // A rebuild moves it, and that is worth one more line.
        let rebuilt = SupervisorBuild {
            version: "0.1.163".into(),
            commit: "def5678".into(),
        };
        observe(
            &store,
            "agent:v1",
            ImageRole::Scout,
            Some(&rebuilt),
            "sess_3",
        )
        .await;
        let notes = store
            .all_events()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| matches!(e.payload, EventPayload::Note { .. }))
            .count();
        assert_eq!(notes, 2);
    }

    /// An image that reports nothing is recorded as reporting nothing, and
    /// reads as the loudest verdict rather than the quietest.
    #[tokio::test]
    async fn an_unstamped_image_is_recorded_and_named() {
        let store = Store::open_in_memory().await.unwrap();
        observe(&store, "builder:v1", ImageRole::Builder, None, "build_1").await;

        let observed = store.image_builds("0.1.163").await.unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].image, "builder:v1");
        assert_eq!(observed[0].role, ImageRole::Builder);
        assert_eq!(observed[0].version, None);
        assert_eq!(
            observed[0].freshness,
            tasks_api::version::ImageFreshness::Unstamped
        );
        assert!(observed[0].freshness.needs_rebuild());
        assert_eq!(observed[0].run_id.as_deref(), Some("build_1"));
    }

    /// The verdict is a comparison against *this* server, not a stored fact —
    /// so the same row reads differently to a newer binary, with no write in
    /// between. That is the whole reason it is not a column.
    #[tokio::test]
    async fn the_verdict_moves_with_the_server_not_with_the_row() {
        let store = Store::open_in_memory().await.unwrap();
        let build = SupervisorBuild {
            version: "0.1.150".into(),
            commit: "abc1234".into(),
        };
        observe(&store, "agent:v1", ImageRole::Scout, Some(&build), "sess_1").await;

        use tasks_api::version::ImageFreshness;
        assert_eq!(
            store.image_builds("0.1.150").await.unwrap()[0].freshness,
            ImageFreshness::Current
        );
        assert_eq!(
            store.image_builds("0.1.163").await.unwrap()[0].freshness,
            ImageFreshness::Behind
        );
        assert_eq!(
            store.image_builds("0.1.140").await.unwrap()[0].freshness,
            ImageFreshness::Ahead
        );
    }
}
