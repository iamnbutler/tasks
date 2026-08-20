//! What pressing `play` will do, said once per install — the generated half of
//! the before-first-`play` sheet (#993), and the record that it was
//! acknowledged.
//!
//! It lives in this crate rather than in `app-gpui` for the two reasons
//! [`crate::paths`] does. More than one client reads it: both app windows can
//! start the pipeline, the menubar can, and a future client that grows a play
//! button must find the same acknowledgement rather than raise its own sheet.
//! And `app-gpui` is not a workspace member, so `make test` never runs its
//! tests — anything here with a rule in it is a rule the suite actually
//! checks. What is left in the app is chrome.
//!
//! Two things this is deliberately not. It is **not a gate**: the sheet is
//! one-shot per install, escapable, and every later `play` is unimpeded — the
//! charter is a kill switch, not a promotion ladder, and pre-approval is the
//! bottleneck the design rejects. And it is **not keyed off the mode**: every
//! boot overwrites the stored mode from `TASKS_DEFAULT_MODE`, so a mode-keyed
//! sheet fires on every restart and gets clicked through, which is the failure
//! the issue names by name.
//!
//! The one distinction that must not be flattened is between "the charter says
//! everything is off" and "the charter could not be read". Both would render
//! as a sheet promising the pipeline will do nothing, on the one surface that
//! exists to warn — so [`Sheet::from_entries`] takes what was actually fetched
//! and [`Sheet::unreadable`] is its own state, with words of its own.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{Capability, CharterEntry, CharterLevel};

/// The acknowledgement's name under the data dir.
pub const ACK_FILE_NAME: &str = "first-play.json";

/// What the sheet says when there is no charter to generate from.
///
/// Never an all-`off` sheet: a fetch that failed and a charter of eleven `off`
/// rows are different facts, and collapsing them lies in the one direction
/// this surface must never lie in.
pub const UNREADABLE_CHARTER: &str = "The charter could not be read, so this list is not \
     available — assume every capability is on until you have checked.";

/// The record that a human has been shown what `play` does on this install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    pub acknowledged_at: DateTime<Utc>,
}

/// `<data dir>/first-play.json`.
pub fn ack_file(data_dir: &Path) -> PathBuf {
    data_dir.join(ACK_FILE_NAME)
}

/// The record, if there is a parseable one.
///
/// A file that is missing, unreadable or corrupt reads as **not**
/// acknowledged — `paths`' hint rule pointed the other way here, and this is
/// the direction it points in for this file: showing the sheet twice costs a
/// click, never showing it is the bug.
pub fn read(data_dir: &Path) -> Option<Acknowledgement> {
    let raw = std::fs::read_to_string(ack_file(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether this install has been shown the sheet.
pub fn acknowledged(data_dir: &Path) -> bool {
    read(data_dir).is_some()
}

/// Write the acknowledgement. Best-effort by contract: a failed write is
/// **not** a refusal — no `$HOME`, a read-only data dir, a full disk — and the
/// caller starts the pipeline anyway and remembers for the session. A sheet
/// that cannot be dismissed permanently is exactly the trained-out-of-use
/// surface this exists to avoid.
pub fn record(data_dir: &Path) -> std::io::Result<Acknowledgement> {
    let ack = Acknowledgement {
        acknowledged_at: Utc::now(),
    };
    std::fs::create_dir_all(data_dir)?;
    let body = serde_json::to_string(&ack)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(ack_file(data_dir), body)?;
    Ok(ack)
}

/// One capability, as the sheet renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetLine {
    pub capability: Capability,
    /// [`Capability::permits`] — the human-facing clause, never the slug.
    pub permits: &'static str,
}

/// The charter, grouped for a reader: what it will do, what it will only
/// narrate, and what is genuinely switched off.
///
/// `off` is rendered rather than filtered out: seeing a switch that is
/// genuinely off is how a person learns the switches are real.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sheet {
    /// `live` — applied, without asking.
    pub live: Vec<SheetLine>,
    /// `shadow` — decided and recorded, nothing applied.
    pub shadow: Vec<SheetLine>,
    /// `off` — refused at the endpoint.
    pub off: Vec<SheetLine>,
    /// No charter was fetched at all. Renders as [`UNREADABLE_CHARTER`] and
    /// never as three empty groups, which would read as "it will do nothing".
    pub unreadable: bool,
}

impl Sheet {
    /// Group a charter the client actually fetched.
    ///
    /// Order is [`Capability::ALL`]'s — additive and reversible first,
    /// irreversible last — so a reader who stops early has already met the
    /// sharp ones. A capability the answer **omits** is `Off`, matching
    /// `Store::charter_entry`, and that is not merely conservative: the server
    /// genuinely refuses a capability it has no row for.
    pub fn from_entries(entries: &[CharterEntry]) -> Self {
        let mut sheet = Sheet::default();
        for capability in Capability::ALL {
            let level = entries
                .iter()
                .find(|entry| entry.capability == capability)
                .map(|entry| entry.level)
                .unwrap_or(CharterLevel::Off);
            let line = SheetLine {
                capability,
                permits: capability.permits(),
            };
            match level {
                CharterLevel::Live => sheet.live.push(line),
                CharterLevel::Shadow => sheet.shadow.push(line),
                CharterLevel::Off => sheet.off.push(line),
            }
        }
        sheet
    }

    /// No charter was readable. Its own state, with no lines at all — never
    /// [`Self::from_entries`] over an empty slice, which is a charter of
    /// eleven `off` rows and a false reassurance.
    pub fn unreadable() -> Self {
        Sheet {
            unreadable: true,
            ..Sheet::default()
        }
    }

    /// The grouping, for a client that renders the whole charter regardless of
    /// how it fetched: `None` when there is nothing to render.
    pub fn from_charter(charter: Option<&[CharterEntry]>) -> Self {
        match charter {
            Some(entries) => Sheet::from_entries(entries),
            None => Sheet::unreadable(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(capability: Capability, level: CharterLevel) -> CharterEntry {
        CharterEntry {
            capability,
            level,
            daily_limit: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn an_acknowledgement_reads_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!acknowledged(dir.path()));
        let written = record(dir.path()).unwrap();
        assert_eq!(read(dir.path()), Some(written));
        assert!(acknowledged(dir.path()));
    }

    /// Showing it twice costs a click; never showing it is the bug.
    #[test]
    fn a_corrupt_record_reads_as_not_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(ack_file(dir.path()), "not json").unwrap();
        assert!(!acknowledged(dir.path()));
    }

    #[test]
    fn every_capability_lands_in_exactly_one_group_in_charter_order() {
        let charter: Vec<_> = Capability::ALL
            .iter()
            .map(|c| entry(*c, CharterLevel::Live))
            .collect();
        let sheet = Sheet::from_entries(&charter);
        assert_eq!(sheet.live.len(), Capability::ALL.len());
        assert!(sheet.shadow.is_empty() && sheet.off.is_empty());
        let order: Vec<_> = sheet.live.iter().map(|line| line.capability).collect();
        assert_eq!(order, Capability::ALL.to_vec());
    }

    #[test]
    fn the_three_levels_group_apart() {
        let sheet = Sheet::from_entries(&[
            entry(Capability::LandBuilds, CharterLevel::Live),
            entry(Capability::CaptureWork, CharterLevel::Shadow),
            entry(Capability::RetireWork, CharterLevel::Off),
        ]);
        assert_eq!(sheet.live.len(), 1);
        assert_eq!(sheet.live[0].capability, Capability::LandBuilds);
        assert_eq!(sheet.shadow.len(), 1);
        // Every capability the answer omitted is `Off`, matching
        // `Store::charter_entry`.
        assert_eq!(sheet.off.len(), Capability::ALL.len() - 2);
        assert!(!sheet.unreadable);
    }

    /// The single most important line in #993: a fetch that failed must never
    /// flow through the same path as a charter of all-`off` rows, because the
    /// sheet would then promise the pipeline will do nothing on exactly the
    /// surface that exists to warn.
    #[test]
    fn an_unreadable_charter_is_its_own_state_and_never_an_all_off_sheet() {
        let unreadable = Sheet::from_charter(None);
        assert!(unreadable.unreadable);
        assert!(unreadable.live.is_empty());
        assert!(unreadable.shadow.is_empty());
        // The tell: it does not claim eleven capabilities are off.
        assert!(unreadable.off.is_empty());

        let all_off = Sheet::from_charter(Some(&[]));
        assert!(!all_off.unreadable);
        assert_eq!(all_off.off.len(), Capability::ALL.len());
        assert_ne!(unreadable, all_off);
    }

    /// The words are what the sheet is for, so the acts that matter are
    /// pinned here rather than left to a fourth reader of `disclaimer.rs`.
    #[test]
    fn the_clauses_name_acts_on_the_readers_own_account() {
        let land = Capability::LandBuilds.permits();
        assert!(land.contains("merge"), "{land}");
        assert!(land.contains("pull request"), "{land}");
        assert!(Capability::RetireWork.permits().contains("close"));
        assert!(
            Capability::CaptureWork
                .permits()
                .contains("your repositories")
        );
    }

    /// A slug is a name for a switch, not a description of an act.
    #[test]
    fn no_clause_renders_its_own_slug() {
        for capability in Capability::ALL {
            let permits = capability.permits();
            assert!(
                !permits.contains(capability.as_str()),
                "{} renders its own slug: {permits}",
                capability.as_str()
            );
            assert!(!permits.trim().is_empty());
        }
    }

    /// `describe` addresses the orchestrator and `permits` addresses a human;
    /// one string doing both jobs is how the sheet ends up telling the reader
    /// to file issues.
    #[test]
    fn permits_is_not_describe() {
        for capability in Capability::ALL {
            assert_ne!(capability.permits(), capability.describe());
        }
    }

    #[test]
    fn the_unreadable_sentence_says_to_assume_the_worst() {
        let text = UNREADABLE_CHARTER.to_lowercase();
        assert!(text.contains("could not be read"));
        assert!(text.contains("assume"));
    }
}
