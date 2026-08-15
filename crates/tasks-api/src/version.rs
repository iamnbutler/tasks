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

use std::cmp::Ordering;

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
    fn supports_judges_against_the_floor() {
        assert_eq!(info("0.1.140").supports("0.1.163"), Support::Current);
        assert_eq!(info("0.1.140").supports("0.1.140"), Support::Current);
        assert_eq!(info("0.1.140").supports("0.1.120"), Support::TooOld);
        assert_eq!(info("0.1.140").supports("unknown"), Support::Unknown);
        assert_eq!(info("nonsense").supports("0.1.163"), Support::Unknown);
    }
}
