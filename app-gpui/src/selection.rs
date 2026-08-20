//! The All Tasks list's selection — the list's *first* selection model, not an
//! extension of one.
//!
//! `Workspace::selected_task` is derived from navigation
//! (`settle_after_nav`, `MiddleView::AllTasks => None`), so it is `None` for
//! the whole time the catalog is on screen. This is the set the catalog's tick
//! boxes build, and it is what the Task menu's verbs fall back to when the
//! navigation selection is absent.
//!
//! Three rules hold its shape, and each is a way this goes quietly wrong:
//!
//! - **Keyed by id, never by index.** Rows appear and disappear behind the
//!   archive toggle and the repo filter, so index N is a different task before
//!   and after — the same reason the row elements are keyed by task.
//! - **Order is never stored.** [`TaskSelection::ordered`] takes it from the
//!   list at the moment it is asked, so the selection cannot disagree with the
//!   list about what comes first, or about what is on screen at all.
//! - **A sweep adds and never removes.** [`TaskSelection::extend_to`] over
//!   rows that are already ticked grows the selection rather than punching a
//!   hole in it, and the anchor stays put across successive sweeps, so
//!   overshooting a range costs one click rather than starting again.

use std::collections::HashSet;

use tasks_client::api::models::TaskId;

/// Which rows of the All Tasks list are ticked, plus where the last plain
/// tick landed — the anchor a ⇧-click sweeps from.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TaskSelection {
    ticked: HashSet<TaskId>,
    /// The last row ticked by a plain click. `None` before the first tick and
    /// after the row it names leaves the list.
    anchor: Option<TaskId>,
}

impl TaskSelection {
    pub fn is_empty(&self) -> bool {
        self.ticked.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ticked.len()
    }

    pub fn contains(&self, id: &TaskId) -> bool {
        self.ticked.contains(id)
    }

    /// Drop everything, anchor included.
    pub fn clear(&mut self) {
        self.ticked.clear();
        self.anchor = None;
    }

    /// A plain click: tick or untick one row, and move the anchor to it.
    ///
    /// The anchor moves even when the click *unticks*, because the anchor is
    /// "where the pointer last committed to a row" rather than "what is
    /// selected" — a sweep after an untick reads from where you just were.
    pub fn toggle(&mut self, id: &TaskId) {
        if !self.ticked.remove(id) {
            self.ticked.insert(id.clone());
        }
        self.anchor = Some(id.clone());
    }

    /// A ⇧-click: add every row between the anchor and `id` inclusive, in the
    /// list's own order.
    ///
    /// Additive by design — see the module comment. With no anchor, or with an
    /// anchor that has left the list, this degrades to a plain tick of `id`
    /// rather than doing nothing, because a ⇧-click that lands on nothing
    /// reads as a broken control.
    pub fn extend_to(&mut self, id: &TaskId, visible: &[TaskId]) {
        let Some(anchor) = self.anchor.clone() else {
            self.toggle_on(id);
            return;
        };
        let (Some(from), Some(to)) = (
            visible.iter().position(|row| row == &anchor),
            visible.iter().position(|row| row == id),
        ) else {
            self.toggle_on(id);
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        for row in &visible[lo..=hi] {
            self.ticked.insert(row.clone());
        }
        // The anchor deliberately stays where it was: successive sweeps from
        // one anchor are how you correct an overshoot in a single click.
    }

    /// Tick `id` if it is not already, and anchor there. The ⇧-click fallback
    /// and nothing else — a plain click is [`Self::toggle`].
    fn toggle_on(&mut self, id: &TaskId) {
        self.ticked.insert(id.clone());
        self.anchor = Some(id.clone());
    }

    /// The selection in the list's order, filtered to what the list is
    /// actually showing.
    ///
    /// Both halves matter: the order is the list's rather than a stored one,
    /// and a ticked row that has left the list is not returned — a verb must
    /// not act on a row nobody can see.
    pub fn ordered(&self, visible: &[TaskId]) -> Vec<TaskId> {
        visible
            .iter()
            .filter(|id| self.ticked.contains(*id))
            .cloned()
            .collect()
    }

    /// Drop whatever has left the list, anchor included.
    ///
    /// Cannot run from the render path (which holds `&self`), so the workspace
    /// calls it from the three places that change what is visible without
    /// changing what is ticked: the `app_state` observer, the archive toggle
    /// and the repo filter.
    pub fn retain_visible(&mut self, visible: &[TaskId]) {
        self.ticked.retain(|id| visible.contains(id));
        if let Some(anchor) = &self.anchor {
            if !visible.contains(anchor) {
                self.anchor = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> TaskId {
        TaskId::from_raw(format!("task-{n}"))
    }

    fn rows(ns: &[u64]) -> Vec<TaskId> {
        ns.iter().copied().map(id).collect()
    }

    fn ticked(selection: &TaskSelection, visible: &[TaskId]) -> Vec<String> {
        selection
            .ordered(visible)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn a_fresh_selection_is_empty() {
        let selection = TaskSelection::default();
        assert!(selection.is_empty());
        assert_eq!(selection.len(), 0);
        assert!(selection.ordered(&rows(&[1, 2])).is_empty());
    }

    #[test]
    fn a_plain_click_ticks_and_unticks() {
        let mut selection = TaskSelection::default();
        selection.toggle(&id(2));
        assert!(selection.contains(&id(2)));
        assert_eq!(selection.len(), 1);
        selection.toggle(&id(2));
        assert!(!selection.contains(&id(2)));
        assert!(selection.is_empty());
    }

    /// The order comes from the list, not from the clicks — so a selection
    /// built bottom-up still reads top-down.
    #[test]
    fn the_order_is_the_lists_and_never_the_click_order() {
        let visible = rows(&[1, 2, 3, 4]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(4));
        selection.toggle(&id(1));
        selection.toggle(&id(3));
        assert_eq!(ticked(&selection, &visible), ["task-1", "task-3", "task-4"]);
    }

    /// The list is the authority on what is on screen: a ticked row the list
    /// no longer holds is not returned, even before pruning has run.
    #[test]
    fn a_row_that_left_the_list_is_not_returned() {
        let mut selection = TaskSelection::default();
        selection.toggle(&id(1));
        selection.toggle(&id(9));
        assert_eq!(ticked(&selection, &rows(&[1, 2, 3])), ["task-1"]);
    }

    #[test]
    fn a_sweep_takes_the_rows_between_the_anchor_and_the_click() {
        let visible = rows(&[1, 2, 3, 4, 5]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(2));
        selection.extend_to(&id(4), &visible);
        assert_eq!(ticked(&selection, &visible), ["task-2", "task-3", "task-4"]);
    }

    /// Upwards is the same range — the anchor is an end, not a start.
    #[test]
    fn a_sweep_works_in_both_directions() {
        let visible = rows(&[1, 2, 3, 4, 5]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(4));
        selection.extend_to(&id(2), &visible);
        assert_eq!(ticked(&selection, &visible), ["task-2", "task-3", "task-4"]);
    }

    /// Additive, and this is the rule with teeth: sweeping over rows that are
    /// already ticked grows the selection rather than punching a hole in it.
    #[test]
    fn a_sweep_adds_and_never_removes() {
        let visible = rows(&[1, 2, 3, 4, 5]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(5));
        selection.toggle(&id(1));
        selection.extend_to(&id(3), &visible);
        assert_eq!(
            ticked(&selection, &visible),
            ["task-1", "task-2", "task-3", "task-5"]
        );
    }

    /// The anchor stays put across successive sweeps, so overshooting a range
    /// costs one click rather than starting again.
    #[test]
    fn the_anchor_survives_a_sweep() {
        let visible = rows(&[1, 2, 3, 4, 5]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(1));
        selection.extend_to(&id(5), &visible);
        selection.extend_to(&id(2), &visible);
        // Still anchored at 1, so the second sweep is 1..=2 — and because
        // sweeps only add, the overshoot's rows are still ticked.
        assert_eq!(
            ticked(&selection, &visible),
            ["task-1", "task-2", "task-3", "task-4", "task-5"]
        );
    }

    /// A ⇧-click with nothing to sweep from is a plain tick, not a no-op: a
    /// control that does nothing reads as broken.
    #[test]
    fn a_sweep_with_no_anchor_is_a_plain_tick() {
        let visible = rows(&[1, 2, 3]);
        let mut selection = TaskSelection::default();
        selection.extend_to(&id(3), &visible);
        assert_eq!(ticked(&selection, &visible), ["task-3"]);
    }

    /// …and so is one whose anchor has left the list behind the toggle.
    #[test]
    fn a_sweep_from_a_departed_anchor_is_a_plain_tick() {
        let mut selection = TaskSelection::default();
        selection.toggle(&id(9));
        selection.extend_to(&id(2), &rows(&[1, 2, 3]));
        assert_eq!(ticked(&selection, &rows(&[1, 2, 3])), ["task-2"]);
    }

    #[test]
    fn pruning_drops_what_has_left_the_list() {
        let mut selection = TaskSelection::default();
        selection.toggle(&id(1));
        selection.toggle(&id(2));
        selection.toggle(&id(3));
        selection.retain_visible(&rows(&[2, 3, 4]));
        assert_eq!(selection.len(), 2);
        assert!(!selection.contains(&id(1)));
    }

    /// Pruning takes the anchor with it, so a later sweep does not run from a
    /// row nobody can see.
    #[test]
    fn pruning_drops_a_departed_anchor() {
        let visible = rows(&[2, 3, 4]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(1));
        selection.retain_visible(&visible);
        selection.extend_to(&id(4), &visible);
        assert_eq!(ticked(&selection, &visible), ["task-4"]);
    }

    #[test]
    fn clearing_takes_the_anchor_too() {
        let visible = rows(&[1, 2, 3]);
        let mut selection = TaskSelection::default();
        selection.toggle(&id(1));
        selection.clear();
        assert!(selection.is_empty());
        selection.extend_to(&id(3), &visible);
        assert_eq!(ticked(&selection, &visible), ["task-3"]);
    }
}
