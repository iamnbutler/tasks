//! The server's build identity, and the one comparison anyone makes with it.
//!
//! `GET /version` answers [`VersionInfo`]: which build is running, and the
//! oldest client build it still expects to speak to. It is the cheapest route
//! on the server — no store, no auth — because its whole job is to answer
//! while everything else might still be wrong.
//!
//! A client that is under the floor is *warned*, never refused: both ends
//! ship from one tree, so the value here is the diagnosis, and a refusal on
//! every route would turn one legible sentence back into the wall of failed
//! requests this exists to replace.
//!
//! The wire enums here carry an inherent `from_str -> Option` rather than
//! implementing `std::str::FromStr`, for the reason stated at the top of
//! [`crate::models`]: callers want an Option to turn into a typed error, not a
//! `FromStr::Err`.
#![allow(clippy::should_implement_trait)]

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Body of `GET /version`. Three flat strings, so `curl … | jq -r .version`
/// is the whole client in a shell script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// `0.1.<commit count>`, or the crate version with no git in reach.
    pub version: String,
    /// Short SHA, `-dirty` for an uncommitted tree, or `unknown`.
    pub commit: String,
    /// The oldest client build this server expects to speak to. Moved by
    /// hand, only when the wire actually breaks — see the server's
    /// `MIN_CLIENT_VERSION`.
    pub min_client_version: String,
}

/// What the server thinks of a client's build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// At or above the floor.
    Current,
    /// Below the floor: the client is stale and should be rebuilt.
    TooOld,
    /// One of the two versions doesn't parse — a build with no git in reach,
    /// most likely. Deliberately not `TooOld`: a warning that fires on builds
    /// that are merely unidentifiable gets trained out of use, and then the
    /// real one goes unread.
    Unknown,
}

impl VersionInfo {
    /// Judge a client build against this server's floor.
    pub fn supports(&self, client_version: &str) -> Support {
        match compare(client_version, &self.min_client_version) {
            Some(Ordering::Less) => Support::TooOld,
            Some(_) => Support::Current,
            None => Support::Unknown,
        }
    }
}

/// Which supervisor a VM image carries, and therefore which half of the
/// pipeline it serves.
///
/// [`Self::from_str`] returns `None` for anything it does not know rather than
/// guessing: a row written by a newer binary is dropped from a report, because
/// showing a builder image as a scout would be worse than omitting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRole {
    Scout,
    Builder,
}

impl ImageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageRole::Scout => "scout",
            ImageRole::Builder => "builder",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "scout" => Some(ImageRole::Scout),
            "builder" => Some(ImageRole::Builder),
            _ => None,
        }
    }
}

/// What a running server thinks of the image a run started in.
///
/// Judged at *read* time and deliberately never stored: this is a comparison
/// against the server's own build, and the server is replaced far more often
/// than the images are, so a stored verdict would be stale the moment the next
/// binary booted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFreshness {
    /// At or above the server's build. Nothing to do.
    Current,
    /// Older than the running server: it is missing whatever has been fixed
    /// since, and nothing inside the pipeline will rebuild it.
    Behind,
    /// Newer than the running server. Deliberately **not** a rebuild request:
    /// rebuilding an image that is already newer changes nothing, and the
    /// thing to move is the server.
    Ahead,
    /// The image reported no identity at all — it was built before there was
    /// one to send.
    ///
    /// Deliberately not [`Self::Unknown`]. Absence here is the *loudest*
    /// reading available: an unstamped image is strictly staler than any
    /// version a supervisor could report, because reporting one is itself the
    /// newer behaviour.
    Unstamped,
    /// One of the two versions does not parse — a build with no git in reach.
    /// Not `Behind`, for the same reason [`Support::Unknown`] is not
    /// [`Support::TooOld`]: a warning that fires on merely unidentifiable
    /// builds gets trained out of use.
    Unknown,
}

impl ImageFreshness {
    /// Judge an image's reported version against the running server's.
    ///
    /// `None` — no version reported — is [`Self::Unstamped`], which is the
    /// whole reason this takes an `Option` rather than being called only when
    /// there is something to compare.
    pub fn judge(image_version: Option<&str>, server_version: &str) -> Self {
        let Some(image_version) = image_version else {
            return ImageFreshness::Unstamped;
        };
        match compare(image_version, server_version) {
            Some(Ordering::Less) => ImageFreshness::Behind,
            Some(Ordering::Equal) => ImageFreshness::Current,
            Some(Ordering::Greater) => ImageFreshness::Ahead,
            None => ImageFreshness::Unknown,
        }
    }

    /// Whether `make images` is the answer.
    ///
    /// `Ahead` is not — see the variant. `Unknown` is not either: it is an
    /// unidentifiable build, not a stale one, and asking for a rebuild that
    /// would produce the same unidentifiable answer teaches a reader to ignore
    /// the line.
    pub fn needs_rebuild(&self) -> bool {
        matches!(self, ImageFreshness::Behind | ImageFreshness::Unstamped)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ImageFreshness::Current => "current",
            ImageFreshness::Behind => "behind",
            ImageFreshness::Ahead => "ahead",
            ImageFreshness::Unstamped => "unstamped",
            ImageFreshness::Unknown => "unknown",
        }
    }
}

/// One VM image, as last observed by a run that started inside it, with the
/// verdict computed against the reporting server's own build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageIdentity {
    /// The image reference the host allocates from, e.g. `agent:v1`.
    pub image: String,
    pub role: ImageRole,
    /// `None` means the supervisor sent no identity — see
    /// [`ImageFreshness::Unstamped`].
    pub version: Option<String>,
    pub commit: Option<String>,
    /// When a run last started in this image. The freshness of the *answer*,
    /// which matters because nothing polls an image — it is only ever observed
    /// by work running inside it.
    pub observed_at: DateTime<Utc>,
    /// The scout session or build that reported it, so the reading can be
    /// traced back to a transcript.
    pub run_id: Option<String>,
    pub freshness: ImageFreshness,
}

/// Order two `0.1.<n>` build versions, or `None` if either doesn't parse.
///
/// Numeric and component-wise, not lexical: the last component is a commit
/// count, so `0.1.100` beats `0.1.9` and string comparison gets that backwards
/// the moment the count crosses a digit boundary. Missing trailing components
/// are zero (`0.1` == `0.1.0`), and a `-suffix` is metadata — it is ignored,
/// so a dirty build compares as the commit it was built from.
///
/// This is not semver and shouldn't grow into it. The commit count is only
/// monotonic along one branch, so two divergent branches can produce equal or
/// misleading numbers; the question it answers well is "did you rebuild after
/// pulling?", which is the question actually being asked.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let a = parse(a)?;
    let b = parse(b)?;
    let len = a.len().max(b.len());
    for index in 0..len {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

fn parse(version: &str) -> Option<Vec<u64>> {
    let core = version.trim().split('-').next()?.trim();
    if core.is_empty() {
        return None;
    }
    core.split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(min: &str) -> VersionInfo {
        VersionInfo {
            version: "0.1.163".into(),
            commit: "abc1234".into(),
            min_client_version: min.into(),
        }
    }

    /// The bug this comparison exists to not have: `"0.1.9" > "0.1.100"` as
    /// strings, and the last component is a commit count that crosses digit
    /// boundaries constantly.
    #[test]
    fn compares_numerically_not_lexically() {
        assert_eq!(compare("0.1.100", "0.1.9"), Some(Ordering::Greater));
        assert_eq!(compare("0.1.9", "0.1.100"), Some(Ordering::Less));
        assert_eq!(compare("0.1.163", "0.1.163"), Some(Ordering::Equal));
    }

    #[test]
    fn missing_trailing_components_are_zero() {
        assert_eq!(compare("0.1", "0.1.0"), Some(Ordering::Equal));
        assert_eq!(compare("0.2", "0.1.999"), Some(Ordering::Greater));
    }

    #[test]
    fn suffix_is_metadata() {
        assert_eq!(compare("0.1.120-dirty", "0.1.120"), Some(Ordering::Equal));
        assert_eq!(
            compare("0.1.0-alpha.1", "0.1.0"),
            Some(Ordering::Equal),
            "everything after the first dash is metadata"
        );
    }

    #[test]
    fn unparseable_is_none() {
        assert_eq!(compare("unknown", "0.1.0"), None);
        assert_eq!(compare("0.1.0", ""), None);
        assert_eq!(compare("0.1.x", "0.1.0"), None);
        assert_eq!(compare("-dirty", "0.1.0"), None);
    }

    #[test]
    fn an_unstamped_image_is_the_loudest_reading_not_the_quietest() {
        // Absence means "built before there was an identity to send", which is
        // strictly staler than any version a supervisor could report.
        assert_eq!(
            ImageFreshness::judge(None, "0.1.163"),
            ImageFreshness::Unstamped
        );
        assert!(ImageFreshness::Unstamped.needs_rebuild());
        assert_ne!(
            ImageFreshness::judge(None, "0.1.163"),
            ImageFreshness::Unknown
        );
    }

    #[test]
    fn freshness_is_judged_against_the_running_server() {
        assert_eq!(
            ImageFreshness::judge(Some("0.1.163"), "0.1.163"),
            ImageFreshness::Current
        );
        assert_eq!(
            ImageFreshness::judge(Some("0.1.100"), "0.1.163"),
            ImageFreshness::Behind
        );
        assert_eq!(
            ImageFreshness::judge(Some("0.1.200"), "0.1.163"),
            ImageFreshness::Ahead
        );
        assert_eq!(
            ImageFreshness::judge(Some("nonsense"), "0.1.163"),
            ImageFreshness::Unknown
        );
        // A dirty stamp compares as the commit it was built from, like
        // everywhere else in this module.
        assert_eq!(
            ImageFreshness::judge(Some("0.1.163-dirty"), "0.1.163"),
            ImageFreshness::Current
        );
    }

    /// Only two verdicts ask for `make images`. Rebuilding an image that is
    /// already newer changes nothing, and an unidentifiable build would come
    /// back just as unidentifiable.
    #[test]
    fn only_behind_and_unstamped_ask_for_a_rebuild() {
        assert!(ImageFreshness::Behind.needs_rebuild());
        assert!(ImageFreshness::Unstamped.needs_rebuild());
        assert!(!ImageFreshness::Current.needs_rebuild());
        assert!(!ImageFreshness::Ahead.needs_rebuild());
        assert!(!ImageFreshness::Unknown.needs_rebuild());
    }

    #[test]
    fn an_unrecognized_role_is_dropped_rather_than_guessed() {
        assert_eq!(ImageRole::from_str("scout"), Some(ImageRole::Scout));
        assert_eq!(ImageRole::from_str("builder"), Some(ImageRole::Builder));
        assert_eq!(ImageRole::from_str("orchestrator"), None);
    }

    #[test]
    fn supports_judges_against_the_floor() {
        assert_eq!(info("0.1.140").supports("0.1.163"), Support::Current);
        assert_eq!(info("0.1.140").supports("0.1.140"), Support::Current);
        assert_eq!(info("0.1.140").supports("0.1.120"), Support::TooOld);
        assert_eq!(info("0.1.140").supports("unknown"), Support::Unknown);
        assert_eq!(info("nonsense").supports("0.1.163"), Support::Unknown);
    }
}
