//! Merge queue — spec Section 7.
//!
//! The pipeline between "an agent finished its work" and "that work ships."

use thiserror::Error;

#[cfg(test)]
use crate::model::merge_queue::ConflictType;
use crate::model::merge_queue::{ConflictInfo, MergeQueueEntry, MergeStatus};
use crate::mode::Mode;

#[derive(Debug, Error)]
pub enum MergeQueueError {
    #[error("entry not found: {0}")]
    NotFound(String),
    #[error("merge queue is held in {0:?} mode")]
    Held(Mode),
}

/// Merge queue — manages the pipeline from completed work to merged code.
///
/// Spec Section 7. Authority is determined by the operating mode:
/// - Stop: nobody (held)
/// - Pause: nobody (held), flush available
/// - Play: orchestrator (continuous)
pub struct MergeQueue {
    entries: Vec<MergeQueueEntry>,
}

impl MergeQueue {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add an entry to the queue (spec Section 7.1 step 3).
    pub fn enqueue(&mut self, entry: MergeQueueEntry) {
        self.entries.push(entry);
    }

    /// Get all entries.
    pub fn entries(&self) -> &[MergeQueueEntry] {
        &self.entries
    }

    /// Get entries with queue positions computed for Approved/Merging entries.
    ///
    /// Positions are 1-indexed and assigned based on `queued_at` order.
    /// Position 1 = next to merge.
    pub fn entries_with_positions(&self) -> Vec<MergeQueueEntry> {
        // First, collect indices of entries that should have positions (Approved or Merging)
        // and sort them by queued_at
        let mut queue_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.status, MergeStatus::Approved | MergeStatus::Merging))
            .map(|(i, _)| i)
            .collect();

        // Sort by queued_at (earliest first = position 1)
        queue_indices.sort_by(|&a, &b| self.entries[a].queued_at.cmp(&self.entries[b].queued_at));

        // Create a map from entry index to position
        let mut position_map: std::collections::HashMap<usize, u32> =
            std::collections::HashMap::new();
        for (pos, &idx) in queue_indices.iter().enumerate() {
            position_map.insert(idx, (pos + 1) as u32);
        }

        // Clone entries and set positions
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut entry = e.clone();
                entry.queue_position = position_map.get(&i).copied();
                entry
            })
            .collect()
    }

    /// Get a mutable entry by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut MergeQueueEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Get an entry by ID.
    pub fn get(&self, id: &str) -> Option<&MergeQueueEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Get an entry by task ID.
    pub fn get_by_task(&self, task_id: &str) -> Option<&MergeQueueEntry> {
        self.entries.iter().find(|e| e.task_id == task_id)
    }

    /// Get an entry by PR URL.
    pub fn get_by_pr_url(&self, pr_url: &str) -> Option<&MergeQueueEntry> {
        self.entries.iter().find(|e| e.pr_url == pr_url)
    }

    /// Get all pending entries (eligible for merge in Play mode).
    pub fn pending(&self) -> Vec<&MergeQueueEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == MergeStatus::Pending)
            .collect()
    }

    /// Get all approved entries (eligible for flush in Pause mode).
    pub fn approved(&self) -> Vec<&MergeQueueEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == MergeStatus::Approved)
            .collect()
    }

    /// Approve an entry (spec Section 7.1 step 4).
    pub fn approve(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Approved;
        Ok(())
    }

    /// Mark an entry as actively merging (GitHub API call in progress).
    pub fn mark_merging(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Merging;
        Ok(())
    }

    /// Reject an entry (spec Section 7.1 step 5).
    pub fn reject(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Rejected;
        entry.completed_at = Some(chrono::Utc::now());
        Ok(())
    }

    /// Mark an entry as merged (spec Section 7.1).
    pub fn mark_merged(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Merged;
        entry.completed_at = Some(chrono::Utc::now());
        Ok(())
    }

    /// Mark an entry as conflicted with optional details (spec Section 7.4).
    pub fn mark_conflict(
        &mut self,
        id: &str,
        conflict_info: Option<ConflictInfo>,
    ) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Conflict;
        entry.conflict_info = conflict_info;
        Ok(())
    }

    /// Clear conflict status and info from an entry (after resolution).
    pub fn clear_conflict(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        if entry.status == MergeStatus::Conflict {
            entry.status = MergeStatus::Pending;
            entry.conflict_info = None;
        }
        Ok(())
    }

    /// Request changes on an entry (spec Section 7.1).
    ///
    /// Unlike reject, the entry stays in the queue and the task gets
    /// priority dispatch to address the feedback.
    pub fn request_changes(
        &mut self,
        id: &str,
        feedback: impl Into<String>,
    ) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::ChangesRequested;
        entry.changes_requested_feedback = Some(feedback.into());
        Ok(())
    }

    /// Get all entries with changes requested.
    pub fn changes_requested(&self) -> Vec<&MergeQueueEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == MergeStatus::ChangesRequested)
            .collect()
    }

    /// Clear changes requested status from an entry (after agent addresses feedback).
    /// Returns the entry to Pending status for re-evaluation.
    pub fn clear_changes_requested(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        if entry.status == MergeStatus::ChangesRequested {
            entry.status = MergeStatus::Pending;
            entry.changes_requested_feedback = None;
        }
        Ok(())
    }

    /// Update the head SHA for an entry.
    /// Returns true if the SHA was updated (i.e., it changed).
    pub fn update_head_sha(&mut self, id: &str, head_sha: &str) -> Result<bool, MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        let changed = entry.head_sha.as_ref().map_or(true, |sha| sha != head_sha);
        if changed {
            entry.head_sha = Some(head_sha.to_string());
        }
        Ok(changed)
    }

    /// Get all entries with conflicts.
    pub fn conflicting(&self) -> Vec<&MergeQueueEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == MergeStatus::Conflict)
            .collect()
    }

    /// Collect approved entries for flush (spec Section 6.2).
    ///
    /// Returns (entry_id, pr_url) pairs of approved entries. Only valid in Pause mode.
    /// The caller is responsible for performing the actual merge and updating status
    /// via mark_merged() or mark_conflict().
    pub fn collect_approved_for_flush(
        &self,
        mode: Mode,
    ) -> Result<Vec<(String, String)>, MergeQueueError> {
        if !mode.flush_available() {
            return Err(MergeQueueError::Held(mode));
        }

        let entries: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|e| e.status == MergeStatus::Approved)
            .map(|e| (e.id.clone(), e.pr_url.clone()))
            .collect();

        Ok(entries)
    }

    /// Remove a specific entry by PR URL. Returns the removed entry if found.
    pub fn remove_by_pr_url(&mut self, pr_url: &str) -> Option<MergeQueueEntry> {
        if let Some(pos) = self.entries.iter().position(|e| e.pr_url == pr_url) {
            Some(self.entries.remove(pos))
        } else {
            None
        }
    }

    /// Remove terminal entries (merged, rejected) from the queue.
    ///
    /// If `merged_cutoff` is provided, only removes Merged/Rejected entries whose
    /// `completed_at` is before the cutoff. This implements a cooldown period to
    /// prevent race conditions where GitHub's merged state hasn't propagated yet,
    /// causing re-polling to create duplicate entries. See issue #438.
    ///
    /// If `conflict_cutoff` is provided, also removes conflict entries that have
    /// been in conflict state since before the cutoff time. This prevents stale
    /// conflicts from accumulating indefinitely. See issue #282.
    pub fn cleanup(
        &mut self,
        merged_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        conflict_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.entries.retain(|e| {
            // Remove merged and rejected entries after cooldown period (issue #438)
            if matches!(e.status, MergeStatus::Merged | MergeStatus::Rejected) {
                match (merged_cutoff, e.completed_at) {
                    // If we have both a cutoff and a completed_at timestamp,
                    // only remove if completed before the cutoff
                    (Some(cutoff), Some(completed)) => {
                        return completed >= cutoff;
                    }
                    // No cutoff provided — remove immediately (legacy behavior)
                    (None, _) => return false,
                    // No completed_at but we have a cutoff — retain for safety
                    // (entry might have been marked merged before the field existed)
                    (Some(_), None) => return false,
                }
            }

            // Optionally remove stale conflict entries
            if let Some(cutoff) = conflict_cutoff {
                if e.status == MergeStatus::Conflict {
                    // If we have conflict_info with a timestamp, use it
                    if let Some(ref info) = e.conflict_info {
                        if info.detected_at < cutoff {
                            return false;
                        }
                    }
                    // No conflict_info means we don't know when the conflict started.
                    // Retain unknown-age conflicts rather than risk premature removal
                    // (e.g., entry queued 30h ago but conflicted 1 minute ago).
                }
            }

            true
        });
    }

    /// Remove entries for the given task IDs. Returns the removed entries.
    pub fn remove_by_task_ids(&mut self, task_ids: &[String]) -> Vec<MergeQueueEntry> {
        let mut removed = Vec::new();
        self.entries.retain(|e| {
            if task_ids.contains(&e.task_id) {
                removed.push(e.clone());
                false
            } else {
                true
            }
        });
        removed
    }
}

impl Default for MergeQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, task_id: &str) -> MergeQueueEntry {
        MergeQueueEntry::new(id, task_id, "https://github.com/test/repo/pull/1")
    }

    #[test]
    fn enqueue_and_list() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        assert_eq!(q.entries().len(), 2);
        assert_eq!(q.pending().len(), 2);
    }

    #[test]
    fn approve_and_collect_for_flush() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));

        q.approve("m1").unwrap();
        assert_eq!(q.approved().len(), 1);

        // collect_approved_for_flush returns entries but doesn't mark them as Merged
        let collected = q.collect_approved_for_flush(Mode::Pause).unwrap();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, "m1");
        // Entry should still be Approved (not Merged) - caller is responsible for marking
        assert_eq!(q.get("m1").unwrap().status, MergeStatus::Approved);
        // m2 stays pending
        assert_eq!(q.get("m2").unwrap().status, MergeStatus::Pending);

        // Caller marks as merged after successful GitHub merge
        q.mark_merged("m1").unwrap();
        assert_eq!(q.get("m1").unwrap().status, MergeStatus::Merged);
    }

    #[test]
    fn collect_for_flush_only_in_pause() {
        let q = MergeQueue::new();

        assert!(q.collect_approved_for_flush(Mode::Stop).is_err());
        assert!(q.collect_approved_for_flush(Mode::Play).is_err());
        assert!(q.collect_approved_for_flush(Mode::Pause).is_ok());
    }

    #[test]
    fn conflict_not_eligible() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.mark_conflict("m1", None).unwrap();
        assert!(q.pending().is_empty());
    }

    #[test]
    fn conflict_with_info() {
        use crate::model::merge_queue::{ConflictInfo, ConflictType};

        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));

        let info = ConflictInfo::new(ConflictType::SourceConflict, "Files conflict");
        q.mark_conflict("m1", Some(info)).unwrap();

        let entry = q.get("m1").unwrap();
        assert_eq!(entry.status, MergeStatus::Conflict);
        assert!(entry.conflict_info.is_some());
        assert_eq!(
            entry.conflict_info.as_ref().unwrap().conflict_type,
            ConflictType::SourceConflict
        );
    }

    #[test]
    fn clear_conflict() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.mark_conflict("m1", None).unwrap();
        assert!(q.pending().is_empty());

        q.clear_conflict("m1").unwrap();
        assert_eq!(q.pending().len(), 1);
    }

    #[test]
    fn list_conflicting() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.mark_conflict("m1", None).unwrap();
        assert_eq!(q.conflicting().len(), 1);
        assert_eq!(q.conflicting()[0].id, "m1");
    }

    #[test]
    fn cleanup_removes_terminal_without_cutoff() {
        // When no merged_cutoff is provided, terminal entries are removed immediately
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.enqueue(entry("m3", "t3"));

        q.mark_merged("m1").unwrap();
        q.reject("m2").unwrap();

        q.cleanup(None, None);
        assert_eq!(q.entries().len(), 1);
        assert_eq!(q.entries()[0].id, "m3");
    }

    #[test]
    fn cleanup_respects_merged_cooldown() {
        use chrono::{Duration, Utc};

        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.enqueue(entry("m3", "t3"));

        // Mark m1 as merged (completed_at = now)
        q.mark_merged("m1").unwrap();

        // Mark m2 as rejected (completed_at = now)
        q.reject("m2").unwrap();

        // Cleanup with 5-minute cutoff — entries completed after cutoff should be retained
        let cutoff = Utc::now() - Duration::minutes(5);
        q.cleanup(Some(cutoff), None);

        // m1 and m2 should still be present (completed within cooldown period)
        // m3 is pending so always retained
        assert_eq!(q.entries().len(), 3);
        assert!(q.get("m1").is_some());
        assert!(q.get("m2").is_some());
        assert!(q.get("m3").is_some());
    }

    #[test]
    fn cleanup_removes_old_merged_entries() {
        use chrono::{Duration, Utc};

        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));

        // Mark m1 as merged
        q.mark_merged("m1").unwrap();

        // Manually backdate the completed_at to simulate an old entry
        if let Some(e) = q.get_mut("m1") {
            e.completed_at = Some(Utc::now() - Duration::minutes(10));
        }

        // m2 stays pending

        // Cleanup with 5-minute cutoff — m1 completed 10 min ago, should be removed
        let cutoff = Utc::now() - Duration::minutes(5);
        q.cleanup(Some(cutoff), None);

        assert_eq!(q.entries().len(), 1);
        assert!(q.get("m1").is_none());
        assert!(q.get("m2").is_some());
    }

    #[test]
    fn cleanup_removes_stale_conflicts() {
        use chrono::{Duration, Utc};

        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.enqueue(entry("m3", "t3"));

        // Mark m1 as conflict with a timestamp in the past
        let old_conflict = ConflictInfo {
            conflict_type: ConflictType::SourceConflict,
            conflicting_files: vec![],
            description: "old conflict".to_string(),
            detected_at: Utc::now() - Duration::hours(48),
        };
        q.mark_conflict("m1", Some(old_conflict)).unwrap();

        // Mark m2 as conflict with a recent timestamp
        let recent_conflict = ConflictInfo {
            conflict_type: ConflictType::SourceConflict,
            conflicting_files: vec![],
            description: "recent conflict".to_string(),
            detected_at: Utc::now() - Duration::hours(1),
        };
        q.mark_conflict("m2", Some(recent_conflict)).unwrap();

        // Cleanup with 24-hour cutoff
        let cutoff = Utc::now() - Duration::hours(24);
        q.cleanup(None, Some(cutoff));

        // m1 (stale conflict) should be removed, m2 (recent conflict) and m3 (pending) should remain
        assert_eq!(q.entries().len(), 2);
        assert!(q.get("m1").is_none());
        assert!(q.get("m2").is_some());
        assert!(q.get("m3").is_some());
    }

    #[test]
    fn cleanup_retains_conflicts_without_info() {
        use chrono::{Duration, Utc};

        let mut q = MergeQueue::new();

        // Create an entry with an old queued_at time but no conflict_info.
        // Without conflict_info we don't know when the conflict actually started,
        // so cleanup must retain it to avoid premature removal.
        let mut old_entry = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1");
        old_entry.queued_at = Utc::now() - Duration::hours(48);
        old_entry.status = MergeStatus::Conflict;
        // No conflict_info — falls back to queued_at
        q.enqueue(old_entry);

        // Create another conflict entry without info
        let mut recent_entry = MergeQueueEntry::new("m2", "t2", "https://github.com/test/repo/pull/2");
        recent_entry.queued_at = Utc::now() - Duration::hours(1);
        recent_entry.status = MergeStatus::Conflict;
        q.enqueue(recent_entry);

        // Cleanup with 24-hour cutoff
        let cutoff = Utc::now() - Duration::hours(24);
        q.cleanup(None, Some(cutoff));

        // Both should be retained â no conflict_info means unknown conflict age
        assert_eq!(q.entries().len(), 2);
        assert!(q.get("m1").is_some());
        assert!(q.get("m2").is_some());
    }

    #[test]
    fn entries_with_positions_basic() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.enqueue(entry("m3", "t3"));

        // Approve m1 and m3, leave m2 pending
        q.approve("m1").unwrap();
        q.approve("m3").unwrap();

        let entries = q.entries_with_positions();
        assert_eq!(entries.len(), 3);

        // m1 is approved first (by queued_at order), so position 1
        let m1 = entries.iter().find(|e| e.id == "m1").unwrap();
        assert_eq!(m1.queue_position, Some(1));

        // m2 is pending, no position
        let m2 = entries.iter().find(|e| e.id == "m2").unwrap();
        assert_eq!(m2.queue_position, None);

        // m3 is approved second, so position 2
        let m3 = entries.iter().find(|e| e.id == "m3").unwrap();
        assert_eq!(m3.queue_position, Some(2));
    }

    #[test]
    fn entries_with_positions_merging_included() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));

        // Approve both, then mark m1 as merging
        q.approve("m1").unwrap();
        q.approve("m2").unwrap();
        q.mark_merging("m1").unwrap();

        let entries = q.entries_with_positions();

        // m1 (merging) should be position 1
        let m1 = entries.iter().find(|e| e.id == "m1").unwrap();
        assert_eq!(m1.status, MergeStatus::Merging);
        assert_eq!(m1.queue_position, Some(1));

        // m2 (approved) should be position 2
        let m2 = entries.iter().find(|e| e.id == "m2").unwrap();
        assert_eq!(m2.status, MergeStatus::Approved);
        assert_eq!(m2.queue_position, Some(2));
    }

    #[test]
    fn entries_with_positions_respects_queued_at_order() {
        use chrono::{Duration, Utc};

        let mut q = MergeQueue::new();

        // Create entries with explicit timestamps (m2 queued before m1)
        let mut e1 = MergeQueueEntry::new("m1", "t1", "https://github.com/test/repo/pull/1");
        e1.queued_at = Utc::now();

        let mut e2 = MergeQueueEntry::new("m2", "t2", "https://github.com/test/repo/pull/2");
        e2.queued_at = Utc::now() - Duration::hours(1); // Queued earlier

        // Enqueue in m1, m2 order (but m2 has earlier timestamp)
        q.enqueue(e1);
        q.enqueue(e2);

        q.approve("m1").unwrap();
        q.approve("m2").unwrap();

        let entries = q.entries_with_positions();

        // m2 was queued earlier, so it should be position 1
        let m1 = entries.iter().find(|e| e.id == "m1").unwrap();
        let m2 = entries.iter().find(|e| e.id == "m2").unwrap();

        assert_eq!(m2.queue_position, Some(1)); // Earlier timestamp = first
        assert_eq!(m1.queue_position, Some(2));
    }

    #[test]
    fn entries_with_positions_no_approved() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));

        // All entries pending, none should have positions
        let entries = q.entries_with_positions();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.queue_position.is_none()));
    }
}
