//! Merge queue — spec Section 7.
//!
//! The pipeline between "an agent finished its work" and "that work ships."

use thiserror::Error;

use crate::model::merge_queue::{MergeQueueEntry, MergeStatus};
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

    /// Mark an entry as conflicted (spec Section 7.4).
    pub fn mark_conflict(&mut self, id: &str) -> Result<(), MergeQueueError> {
        let entry = self
            .get_mut(id)
            .ok_or_else(|| MergeQueueError::NotFound(id.to_string()))?;
        entry.status = MergeStatus::Conflict;
        Ok(())
    }

    /// Flush: push through all approved entries (spec Section 6.2).
    ///
    /// Returns the IDs of entries that were flushed. Only valid in Pause mode.
    pub fn flush(&mut self, mode: Mode) -> Result<Vec<String>, MergeQueueError> {
        if !mode.flush_available() {
            return Err(MergeQueueError::Held(mode));
        }

        let ids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.status == MergeStatus::Approved)
            .map(|e| e.id.clone())
            .collect();

        for entry in &mut self.entries {
            if entry.status == MergeStatus::Approved {
                entry.status = MergeStatus::Merged;
            }
        }

        Ok(ids)
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
    fn approve_and_flush() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.enqueue(entry("m2", "t2"));

        q.approve("m1").unwrap();
        assert_eq!(q.approved().len(), 1);

        let flushed = q.flush(Mode::Pause).unwrap();
        assert_eq!(flushed, vec!["m1"]);
        assert_eq!(q.get("m1").unwrap().status, MergeStatus::Merged);
        // m2 stays pending
        assert_eq!(q.get("m2").unwrap().status, MergeStatus::Pending);
    }

    #[test]
    fn flush_only_in_pause() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.approve("m1").unwrap();

        assert!(q.flush(Mode::Stop).is_err());
        assert!(q.flush(Mode::Play).is_err());
        assert!(q.flush(Mode::Pause).is_ok());
    }

    #[test]
    fn conflict_not_eligible() {
        let mut q = MergeQueue::new();
        q.enqueue(entry("m1", "t1"));
        q.mark_conflict("m1").unwrap();
        assert!(q.pending().is_empty());
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
