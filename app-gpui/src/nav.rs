//! Middle-column navigation: what the center pane shows, with browser-style
//! back/forward history.
//!
//! Deliberately gpui-free, like [`crate::chat_log`]: the app's tests are pure
//! functions over view state, so everything the ‹ › chevrons and ⌘[ / ⌘] need
//! to be correct about lives here, where it can be asserted without a window.
//!
//! History semantics are a browser's: navigating somewhere new truncates the
//! forward stack, going back keeps the abandoned entry reachable with forward,
//! and re-selecting the current view is a no-op rather than a duplicate entry
//! (arrowing through the task tree must not bury the history under copies).

use tasks_client::api::models::TaskId;

/// What the middle column shows. The whole navigable surface — sections died
/// with the v3 frame swap, so there is nothing else to be "at".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiddleView {
    /// The full catalog — the one place backlog lives, and the table today's
    /// Tasks section becomes.
    AllTasks,
    /// One task's tab set.
    Task(TaskId),
}

/// The tabs a selected task offers. Overview is the default on every fresh
/// selection — the landing tab, per the v3 spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskTab {
    #[default]
    Overview,
    Brief,
    AgentFeed,
    Changes,
}

impl TaskTab {
    pub const ALL: [TaskTab; 4] = [
        TaskTab::Overview,
        TaskTab::Brief,
        TaskTab::AgentFeed,
        TaskTab::Changes,
    ];

    /// Stable element id for the tab strip's rows.
    pub fn id(self) -> &'static str {
        match self {
            TaskTab::Overview => "overview",
            TaskTab::Brief => "brief",
            TaskTab::AgentFeed => "agent-feed",
            TaskTab::Changes => "changes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskTab::Overview => "Overview",
            TaskTab::Brief => "Brief",
            TaskTab::AgentFeed => "Agent Feed",
            TaskTab::Changes => "Changes",
        }
    }
}

/// How much history is kept. Beyond this the oldest entries fall off; nobody
/// presses back fifty times, and an unbounded stack in a long-lived window is
/// a leak with a story attached.
const MAX_HISTORY: usize = 50;

/// The middle column's position and its back/forward stacks.
#[derive(Debug)]
pub struct NavHistory {
    back: Vec<MiddleView>,
    current: MiddleView,
    forward: Vec<MiddleView>,
}

impl Default for NavHistory {
    /// A window opens on the catalog — the view that exists whatever the
    /// server holds.
    fn default() -> Self {
        Self {
            back: Vec::new(),
            current: MiddleView::AllTasks,
            forward: Vec::new(),
        }
    }
}

impl NavHistory {
    pub fn current(&self) -> &MiddleView {
        &self.current
    }

    /// Go somewhere. Returns whether the position actually moved — a no-op
    /// re-selection needs no re-render and must not truncate the forward
    /// stack (dismissing a menu over the current view is not a navigation).
    pub fn navigate(&mut self, to: MiddleView) -> bool {
        if to == self.current {
            return false;
        }
        self.back.push(std::mem::replace(&mut self.current, to));
        if self.back.len() > MAX_HISTORY {
            self.back.remove(0);
        }
        self.forward.clear();
        true
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Step back, if there is anywhere to step to. Returns whether it moved.
    pub fn back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        self.forward
            .push(std::mem::replace(&mut self.current, previous));
        true
    }

    /// Step forward, undoing the most recent [`Self::back`].
    pub fn forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        self.back.push(std::mem::replace(&mut self.current, next));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(n: u8) -> MiddleView {
        MiddleView::Task(TaskId::from_raw(format!("task_{n}")))
    }

    #[test]
    fn opens_on_the_catalog() {
        let nav = NavHistory::default();
        assert_eq!(nav.current(), &MiddleView::AllTasks);
        assert!(!nav.can_go_back());
        assert!(!nav.can_go_forward());
    }

    #[test]
    fn navigating_to_the_current_view_is_a_no_op() {
        let mut nav = NavHistory::default();
        assert!(nav.navigate(task(1)));
        assert!(!nav.navigate(task(1)), "re-selection must not move");
        assert!(nav.can_go_back());
        // …and must not have stacked a duplicate: one back lands on AllTasks.
        assert!(nav.back());
        assert_eq!(nav.current(), &MiddleView::AllTasks);
        assert!(!nav.can_go_back());
    }

    #[test]
    fn back_and_forward_walk_the_same_path() {
        let mut nav = NavHistory::default();
        nav.navigate(task(1));
        nav.navigate(task(2));
        assert!(nav.back());
        assert_eq!(nav.current(), &task(1));
        assert!(nav.back());
        assert_eq!(nav.current(), &MiddleView::AllTasks);
        assert!(!nav.back(), "nothing before the opening view");
        assert!(nav.forward());
        assert!(nav.forward());
        assert_eq!(nav.current(), &task(2));
        assert!(!nav.can_go_forward());
    }

    /// The browser rule: going back and then somewhere new abandons the
    /// forward branch — forward must not resurrect it.
    #[test]
    fn a_new_navigation_truncates_the_forward_stack() {
        let mut nav = NavHistory::default();
        nav.navigate(task(1));
        nav.navigate(task(2));
        nav.back();
        assert!(nav.can_go_forward());
        nav.navigate(task(3));
        assert!(!nav.can_go_forward());
        assert!(nav.back());
        assert_eq!(nav.current(), &task(1));
    }

    /// A no-op navigate is genuinely nothing: with a forward stack live, a
    /// re-selection of the current view must not clear it.
    #[test]
    fn a_no_op_navigate_keeps_the_forward_stack() {
        let mut nav = NavHistory::default();
        nav.navigate(task(1));
        nav.back();
        assert!(nav.can_go_forward());
        assert!(!nav.navigate(MiddleView::AllTasks));
        assert!(nav.can_go_forward(), "no movement, no truncation");
    }

    #[test]
    fn history_is_bounded() {
        let mut nav = NavHistory::default();
        for n in 0..(MAX_HISTORY as u16 + 20) {
            nav.navigate(MiddleView::Task(TaskId::from_raw(format!("task_{n}"))));
        }
        let mut steps = 0;
        while nav.back() {
            steps += 1;
        }
        assert_eq!(steps, MAX_HISTORY, "oldest entries fall off, no more");
    }

    /// Tab element ids sit at the root of an id path (#861's collision
    /// class), so they must be distinct.
    #[test]
    fn tab_ids_are_distinct() {
        let mut ids: Vec<_> = TaskTab::ALL.iter().map(|tab| tab.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TaskTab::ALL.len());
    }
}
