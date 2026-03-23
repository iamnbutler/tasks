//! Merge queue — spec Section 7.
//!
//! The pipeline between "an agent finished its work" and "that work ships."

use thiserror::Error;

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

    /// Reject an entry (spec Section 7.1 step 5).
    pub fn reject(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Rejected;
        Ok(())
    }

    /// Mark an entry as merged (spec Section 7.1).
    pub fn mark_merged(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Merged;
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
    pub fn cleanup(&mut self) {
        self.entries.retain(|e| {
            !matches!(e.status, MergeStatus::Merged | MergeStatus::Rejected)
        });
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
    fn cleanup_removes_terminal() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));
        q.enqueue(entry("m3", "t3"));

        q.mark_merged("m1").unwrap();
        q.reject("m2").unwrap();

        q.cleanup();
        assert_eq!(q.entries().len(), 1);
        assert_eq!(q.entries()[0].id, "m3");
    }
}
