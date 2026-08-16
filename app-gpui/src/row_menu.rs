//! What a right-click on a task row offers.
//!
//! [`entries`] is a pure function of [`RowContext`], deliberately mirroring
//! `menus::menus(MenuState) -> Vec<Menu>`: that split is what makes "which
//! verbs exist, which are greyed, and why" testable without standing up a gpui
//! `App`. The workspace turns the result into gpuikit menu items and hangs the
//! handlers off them; nothing here knows what a window is.
//!
//! Two rules the shape follows, both worth stating because they cost
//! something:
//!
//! - **Every row offers the same verbs in the same order.** A menu whose items
//!   move with the row's state is a menu you have to read before you can use
//!   it. What changes is which are greyed.
//! - **Disabled, not absent, and the reason rides in the label.** gpuikit's
//!   `MenuItem` has no tooltip and no submenu, so `"Scout Now (already
//!   running)"` is the only place a reason can go — and the lack of submenus is
//!   why closing an issue is two flat items rather than a reason picker.
//!
//! Legality is the server's to enforce; this only mirrors it, so a verb that
//! slips through greying still comes back as the server's own message in the
//! banner. The predicates below are the store's: `queue_task` requires
//! `Backlog`, `dequeue_task` requires `Queued`, `push_task_to_front` takes
//! either, `create_build` takes only an approved spec, and a review verdict
//! needs a `pending_review` entry.

use tasks_client::api::models::{GhState, SpecQueueStatus, TaskState};

use crate::menus;

/// Everything the menu's shape depends on — an owned copy, taken before any
/// listener is built, so no borrow of the app state outlives the click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowContext {
    pub task_state: TaskState,
    /// Open or closed *upstream*. Lags a close by up to a poll interval; see
    /// [`RowAction::Reopen`].
    pub gh_state: GhState,
    /// The task's project is known, so an issue URL can be formed.
    pub has_github_url: bool,
    /// Status of the queue entry for the task's latest spec. `None` when the
    /// task has no spec yet, or has one that never reached the queue.
    pub spec: Option<SpecQueueStatus>,
}

/// One verb. Exhaustive on purpose: adding a variant is a compile error in
/// `Workspace::perform_row_action` until it is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Queue,
    Dequeue,
    ScoutNow,
    /// Stop the scout or build this row currently has in flight. Destructive:
    /// the VM and everything on its disk go away, and only what the run had
    /// already checkpointed survives.
    CancelRun,
    ApproveSpec,
    /// Reveals and focuses the inspector's review composer. The one item that
    /// needs text, and the only one that opens anything — hence the ellipsis.
    ReviewSpec,
    RequestBuild,
    CloseCompleted,
    CloseNotPlanned,
    /// Never greyed. `gh_state` lags a close by up to a poll interval (close
    /// returns 202 and applies nothing locally), so greying the undo on that
    /// stale fact would hide it during exactly the window someone wants it.
    Reopen,
    OpenOnGitHub,
    CopyNumber,
    CopyUrl,
}

/// One item as the menu will render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowItem {
    pub action: RowAction,
    /// Stable handle for the item, for tests and element ids.
    pub id: &'static str,
    /// The verb alone. [`RowItem::menu_label`] is what gets rendered.
    pub label: &'static str,
    /// Why this cannot run right now — `None` when it can.
    pub disabled: Option<&'static str>,
    /// Rendered in the theme's danger colour: this one leaves the app and
    /// changes something on GitHub.
    pub destructive: bool,
    /// The key equivalent this verb also answers to, rendered from the
    /// keystroke `menus` actually binds rather than written out again — so
    /// the advertised shortcut cannot drift from the bound one.
    pub kbd: Option<String>,
}

impl RowItem {
    /// The label as rendered: the verb, plus the refusal when there is one.
    pub fn menu_label(&self) -> String {
        match self.disabled {
            Some(reason) => format!("{} ({reason})", self.label),
            None => self.label.to_string(),
        }
    }
}

/// An item or a rule between groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEntry {
    Item(RowItem),
    Separator,
}

/// Every verb a task row offers, in a fixed order, greyed by `context`.
pub fn entries(context: RowContext) -> Vec<RowEntry> {
    let item = |action, id, label, disabled, destructive, kbd| {
        RowEntry::Item(RowItem {
            action,
            id,
            label,
            disabled,
            destructive,
            kbd,
        })
    };

    vec![
        // Picking work up, and putting it back down.
        item(
            RowAction::Queue,
            "row-queue",
            "Add to Queue",
            queue_refusal(context),
            false,
            Some(menus::rendered_keystroke(menus::QUEUE_KEYSTROKE)),
        ),
        item(
            RowAction::Dequeue,
            "row-dequeue",
            "Remove from Queue",
            dequeue_refusal(context),
            false,
            None,
        ),
        item(
            RowAction::ScoutNow,
            "row-scout",
            "Scout Now",
            scout_refusal(context),
            false,
            Some(menus::rendered_keystroke(menus::SCOUT_KEYSTROKE)),
        ),
        // Beside the verbs that start work, because it is the answer to having
        // started it. Destructive and unbound to a key for the same reason the
        // closing verbs are: a mis-keyed cancel throws away a VM hour.
        item(
            RowAction::CancelRun,
            "row-cancel",
            "Cancel Run",
            cancel_refusal(context),
            true,
            None,
        ),
        RowEntry::Separator,
        // The spec, and what a verdict on it leads to.
        item(
            RowAction::ApproveSpec,
            "row-approve",
            "Approve Spec",
            verdict_refusal(context),
            false,
            Some(menus::rendered_keystroke(menus::APPROVE_KEYSTROKE)),
        ),
        item(
            RowAction::ReviewSpec,
            "row-review",
            "Review Spec…",
            verdict_refusal(context),
            false,
            None,
        ),
        item(
            RowAction::RequestBuild,
            "row-build",
            "Request Build",
            build_refusal(context),
            false,
            None,
        ),
        RowEntry::Separator,
        // Retiring work, with its undo directly underneath. One click and no
        // confirm, which is this system's own safety argument — ledger plus
        // recourse, not pre-approval — rather than a shortcut past it.
        item(
            RowAction::CloseCompleted,
            "row-close-completed",
            "Close as Completed",
            close_refusal(context),
            true,
            None,
        ),
        item(
            RowAction::CloseNotPlanned,
            "row-close-not-planned",
            "Close as Not Planned",
            close_refusal(context),
            true,
            None,
        ),
        item(
            RowAction::Reopen,
            "row-reopen",
            "Reopen Issue",
            None,
            false,
            None,
        ),
        RowEntry::Separator,
        item(
            RowAction::OpenOnGitHub,
            "row-open-github",
            "Open on GitHub",
            github_refusal(context),
            false,
            None,
        ),
        item(
            RowAction::CopyNumber,
            "row-copy-number",
            "Copy Issue Number",
            None,
            false,
            None,
        ),
        item(
            RowAction::CopyUrl,
            "row-copy-url",
            "Copy Issue URL",
            github_refusal(context),
            false,
            None,
        ),
    ]
}

/// The item for `action`, whatever the state — the menu always offers all of
/// them, so this never comes back `None`.
///
/// How the keyboard path re-derives legality without a menu: the item carries
/// both the verb's name and why it cannot run, which is what the banner needs
/// to say. A keystroke that quietly does nothing reads as a bug; the reason
/// reads as an answer.
pub fn item(context: RowContext, action: RowAction) -> Option<RowItem> {
    entries(context).into_iter().find_map(|entry| match entry {
        RowEntry::Item(item) if item.action == action => Some(item),
        _ => None,
    })
}

fn queue_refusal(context: RowContext) -> Option<&'static str> {
    match context.task_state {
        TaskState::Backlog => None,
        TaskState::Queued => Some("already queued"),
        TaskState::Scouting => Some("a scout is running"),
        TaskState::InReview => Some("its spec is in review"),
        TaskState::ReadyToBuild => Some("its spec is approved"),
        TaskState::Building => Some("a build is running"),
        TaskState::Done => Some("this task is done"),
        TaskState::Rejected => Some("this task was rejected"),
    }
}

fn dequeue_refusal(context: RowContext) -> Option<&'static str> {
    match context.task_state {
        TaskState::Queued => None,
        TaskState::Backlog => Some("not queued"),
        TaskState::Scouting => Some("a scout is running"),
        // Work past `Queued` cannot be un-picked: it stays picked up, and a
        // spec's fate is decided by reviewing it.
        TaskState::InReview | TaskState::ReadyToBuild | TaskState::Building => {
            Some("work has already started")
        }
        TaskState::Done => Some("this task is done"),
        TaskState::Rejected => Some("this task was rejected"),
    }
}

fn scout_refusal(context: RowContext) -> Option<&'static str> {
    match context.task_state {
        TaskState::Backlog | TaskState::Queued => None,
        TaskState::Scouting => Some("already running"),
        TaskState::InReview => Some("its spec is in review"),
        TaskState::ReadyToBuild => Some("its spec is approved"),
        TaskState::Building => Some("a build is running"),
        TaskState::Done => Some("this task is done"),
        TaskState::Rejected => Some("this task was rejected"),
    }
}

/// Cancelling needs something to cancel: a scout or a build actually in
/// flight, which from a task row is exactly `Scouting` and `Building`.
///
/// A queued build is *not* offered here even though the server accepts one:
/// its task still reads `ready_to_build`, so the row cannot tell it from a
/// task with nothing dispatched, and a verb that sometimes stops a build and
/// sometimes says "nothing is running" is worse than one that says so plainly.
fn cancel_refusal(context: RowContext) -> Option<&'static str> {
    match context.task_state {
        TaskState::Scouting | TaskState::Building => None,
        TaskState::Backlog | TaskState::Queued => Some("nothing has started yet"),
        TaskState::InReview | TaskState::ReadyToBuild => Some("nothing is running"),
        TaskState::Done => Some("this task is done"),
        TaskState::Rejected => Some("this task was rejected"),
    }
}

/// A verdict — approve, or open the review form — needs a spec waiting for one.
fn verdict_refusal(context: RowContext) -> Option<&'static str> {
    match context.spec {
        Some(SpecQueueStatus::PendingReview) => None,
        None => Some("no spec yet"),
        Some(SpecQueueStatus::Approved) => Some("already approved"),
        Some(SpecQueueStatus::NeedsRevision) => Some("sent back for revision"),
        Some(SpecQueueStatus::Blocked) => Some("blocked"),
        Some(SpecQueueStatus::Rejected) => Some("rejected"),
        Some(SpecQueueStatus::Built) => Some("already built"),
    }
}

fn build_refusal(context: RowContext) -> Option<&'static str> {
    match context.spec {
        // The store refuses a spec that is already in a queued or running
        // build; `Building` is how that reads from a task row.
        Some(SpecQueueStatus::Approved) => match context.task_state {
            TaskState::Building => Some("a build is running"),
            _ => None,
        },
        None => Some("no spec yet"),
        Some(SpecQueueStatus::PendingReview) => Some("its spec is not approved yet"),
        Some(SpecQueueStatus::NeedsRevision) => Some("sent back for revision"),
        Some(SpecQueueStatus::Blocked) => Some("blocked"),
        Some(SpecQueueStatus::Rejected) => Some("rejected"),
        Some(SpecQueueStatus::Built) => Some("already built"),
    }
}

fn close_refusal(context: RowContext) -> Option<&'static str> {
    match context.gh_state {
        GhState::Open => None,
        GhState::Closed => Some("the issue is already closed"),
    }
}

fn github_refusal(context: RowContext) -> Option<&'static str> {
    match context.has_github_url {
        true => None,
        false => Some("no project for this task"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(task_state: TaskState) -> RowContext {
        RowContext {
            task_state,
            gh_state: GhState::Open,
            has_github_url: true,
            spec: None,
        }
    }

    fn with_spec(task_state: TaskState, spec: SpecQueueStatus) -> RowContext {
        RowContext {
            spec: Some(spec),
            ..context(task_state)
        }
    }

    fn items(context: RowContext) -> Vec<RowItem> {
        entries(context)
            .into_iter()
            .filter_map(|entry| match entry {
                RowEntry::Item(item) => Some(item),
                RowEntry::Separator => None,
            })
            .collect()
    }

    fn enabled(context: RowContext) -> Vec<RowAction> {
        items(context)
            .into_iter()
            .filter(|item| item.disabled.is_none())
            .map(|item| item.action)
            .collect()
    }

    /// Why `action` cannot run on this row, or `None` when it can.
    fn refusal(context: RowContext, action: RowAction) -> Option<&'static str> {
        item(context, action).and_then(|item| item.disabled)
    }

    /// Every combination of row state there is — 8 task states x 2 GitHub
    /// states x 2 project-known x 7 spec statuses (including none).
    fn every_context() -> Vec<RowContext> {
        let states = [
            TaskState::Backlog,
            TaskState::Queued,
            TaskState::Scouting,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Building,
            TaskState::Done,
            TaskState::Rejected,
        ];
        let specs = [
            None,
            Some(SpecQueueStatus::PendingReview),
            Some(SpecQueueStatus::Approved),
            Some(SpecQueueStatus::NeedsRevision),
            Some(SpecQueueStatus::Blocked),
            Some(SpecQueueStatus::Rejected),
            Some(SpecQueueStatus::Built),
        ];
        let mut all = Vec::new();
        for task_state in states {
            for gh_state in [GhState::Open, GhState::Closed] {
                for has_github_url in [true, false] {
                    for spec in specs {
                        all.push(RowContext {
                            task_state,
                            gh_state,
                            has_github_url,
                            spec,
                        });
                    }
                }
            }
        }
        all
    }

    /// The menu is the same list every time; only the greying moves. A menu
    /// whose items shuffle with state is one you have to read before using.
    #[test]
    fn every_row_offers_the_same_verbs_in_the_same_order() {
        let reference: Vec<RowAction> = items(context(TaskState::Backlog))
            .into_iter()
            .map(|item| item.action)
            .collect();
        assert_eq!(reference.len(), 13);

        for context in every_context() {
            let actions: Vec<RowAction> =
                items(context).into_iter().map(|item| item.action).collect();
            assert_eq!(actions, reference, "{context:?}");
        }
    }

    /// No state can produce a verb twice — the menu is a fixed list, not a
    /// set of conditional groups that can both appear.
    #[test]
    fn no_state_offers_the_same_verb_twice() {
        for context in every_context() {
            let mut ids: Vec<&str> = items(context).into_iter().map(|item| item.id).collect();
            let total = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), total, "{context:?}");
        }
    }

    /// Greyed means "and here is why" — never a dead item with no answer.
    #[test]
    fn a_greyed_item_always_says_why() {
        for context in every_context() {
            for item in items(context) {
                if let Some(reason) = item.disabled {
                    assert!(!reason.is_empty(), "{item:?}");
                    assert_eq!(item.menu_label(), format!("{} ({reason})", item.label));
                } else {
                    assert_eq!(item.menu_label(), item.label);
                }
            }
        }
    }

    #[test]
    fn queueing_follows_the_stores_own_predicates() {
        assert!(enabled(context(TaskState::Backlog)).contains(&RowAction::Queue));
        assert!(!enabled(context(TaskState::Queued)).contains(&RowAction::Queue));

        // `dequeue_task` requires exactly `Queued`.
        assert!(enabled(context(TaskState::Queued)).contains(&RowAction::Dequeue));
        assert!(!enabled(context(TaskState::Backlog)).contains(&RowAction::Dequeue));

        // `push_task_to_front` takes either, and nothing else.
        for state in [TaskState::Backlog, TaskState::Queued] {
            assert!(
                enabled(context(state)).contains(&RowAction::ScoutNow),
                "{state:?}"
            );
        }
        for state in [
            TaskState::Scouting,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Building,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            assert!(
                !enabled(context(state)).contains(&RowAction::ScoutNow),
                "{state:?}"
            );
        }
    }

    /// A scout already running is the one refusal worth naming exactly — it
    /// is the state someone is most likely to try the verb from.
    #[test]
    fn a_running_scout_says_so_in_the_label() {
        let item = item(context(TaskState::Scouting), RowAction::ScoutNow).unwrap();
        assert_eq!(item.menu_label(), "Scout Now (already running)");
    }

    /// Approve and Review are one verdict in two shapes: they open and close
    /// together, on a spec that is actually waiting for one.
    #[test]
    fn a_verdict_needs_a_spec_pending_review() {
        let pending = with_spec(TaskState::InReview, SpecQueueStatus::PendingReview);
        assert!(enabled(pending).contains(&RowAction::ApproveSpec));
        assert!(enabled(pending).contains(&RowAction::ReviewSpec));

        for status in [
            SpecQueueStatus::Approved,
            SpecQueueStatus::NeedsRevision,
            SpecQueueStatus::Blocked,
            SpecQueueStatus::Rejected,
            SpecQueueStatus::Built,
        ] {
            let context = with_spec(TaskState::InReview, status);
            assert!(
                !enabled(context).contains(&RowAction::ApproveSpec),
                "{status:?}"
            );
            assert!(
                !enabled(context).contains(&RowAction::ReviewSpec),
                "{status:?}"
            );
        }
        // No spec at all is the ordinary case, and gets its own reason.
        assert_eq!(
            refusal(context(TaskState::Backlog), RowAction::ApproveSpec),
            Some("no spec yet")
        );
    }

    /// `create_build` takes approved specs only, and refuses one already in a
    /// queued or running build.
    #[test]
    fn only_an_approved_spec_can_be_built() {
        assert!(enabled(with_spec(
            TaskState::ReadyToBuild,
            SpecQueueStatus::Approved
        ))
        .contains(&RowAction::RequestBuild));
        assert!(
            !enabled(with_spec(TaskState::Building, SpecQueueStatus::Approved))
                .contains(&RowAction::RequestBuild)
        );
        assert!(!enabled(with_spec(
            TaskState::InReview,
            SpecQueueStatus::PendingReview
        ))
        .contains(&RowAction::RequestBuild));
    }

    /// Closing is offered on an open issue and greyed on a closed one — and
    /// as two flat items, because gpuikit's menu has no submenu to hang a
    /// reason picker off.
    #[test]
    fn closing_offers_both_reasons_and_greys_on_a_closed_issue() {
        let open = context(TaskState::Backlog);
        assert!(enabled(open).contains(&RowAction::CloseCompleted));
        assert!(enabled(open).contains(&RowAction::CloseNotPlanned));

        let closed = RowContext {
            gh_state: GhState::Closed,
            ..open
        };
        assert_eq!(
            refusal(closed, RowAction::CloseCompleted),
            Some("the issue is already closed")
        );
        assert_eq!(
            refusal(closed, RowAction::CloseNotPlanned),
            Some("the issue is already closed")
        );
    }

    /// `gh_state` lags a close by up to a poll interval, because close returns
    /// 202 and applies nothing locally. Greying the undo on that stale fact
    /// would hide it during exactly the window someone wants it.
    #[test]
    fn reopen_is_never_greyed() {
        for context in every_context() {
            assert_eq!(refusal(context, RowAction::Reopen), None, "{context:?}");
        }
    }

    /// Nothing that leaves the app is offered without somewhere to go.
    #[test]
    fn the_github_verbs_need_a_url() {
        let no_project = RowContext {
            has_github_url: false,
            ..context(TaskState::Backlog)
        };
        assert!(!enabled(no_project).contains(&RowAction::OpenOnGitHub));
        assert!(!enabled(no_project).contains(&RowAction::CopyUrl));
        // The number lives on the task itself, so it is always copyable.
        assert!(enabled(no_project).contains(&RowAction::CopyNumber));
    }

    /// Cancelling is offered exactly where something is in flight — the two
    /// states a run can be interrupted from — and says why everywhere else.
    #[test]
    fn cancelling_needs_a_run_in_flight() {
        for state in [TaskState::Scouting, TaskState::Building] {
            assert!(
                enabled(context(state)).contains(&RowAction::CancelRun),
                "{state:?}"
            );
        }
        for state in [
            TaskState::Backlog,
            TaskState::Queued,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            assert!(
                !enabled(context(state)).contains(&RowAction::CancelRun),
                "{state:?}"
            );
        }
        assert_eq!(
            item(context(TaskState::Scouting), RowAction::CancelRun)
                .unwrap()
                .menu_label(),
            "Cancel Run"
        );
        assert_eq!(
            refusal(context(TaskState::Queued), RowAction::CancelRun),
            Some("nothing has started yet")
        );
    }

    /// The verbs that throw work away are rendered as such. Cancel joins the
    /// closing pair: it destroys a VM and everything on its disk.
    #[test]
    fn the_destructive_verbs_are_marked_destructive() {
        let destructive: Vec<RowAction> = items(context(TaskState::Scouting))
            .into_iter()
            .filter(|item| item.destructive)
            .map(|item| item.action)
            .collect();
        assert_eq!(
            destructive,
            [
                RowAction::CancelRun,
                RowAction::CloseCompleted,
                RowAction::CloseNotPlanned
            ]
        );
    }

    /// Copying is never a write, so it is never greyed by pipeline state.
    #[test]
    fn copying_the_number_survives_every_state() {
        for context in every_context() {
            assert_eq!(refusal(context, RowAction::CopyNumber), None, "{context:?}");
        }
    }

    /// Only the three safe verbs carry a key equivalent. Nothing that closes
    /// an issue is bound, for the reason `menus.rs` gives for not binding a
    /// server restart.
    #[test]
    fn only_safe_verbs_advertise_a_shortcut() {
        let bound: Vec<RowAction> = items(context(TaskState::Backlog))
            .into_iter()
            .filter(|item| item.kbd.is_some())
            .map(|item| item.action)
            .collect();
        assert_eq!(
            bound,
            [
                RowAction::Queue,
                RowAction::ScoutNow,
                RowAction::ApproveSpec
            ]
        );
    }

    /// The menu advertises what `menus` actually binds — it renders the
    /// keystroke rather than restating it, so the only thing left to pin is
    /// what those three keystrokes come out as on screen.
    #[test]
    fn the_advertised_shortcuts_are_the_ones_menus_binds() {
        for (action, shown) in [
            (RowAction::Queue, "⇧⌘U"),
            (RowAction::ScoutNow, "⇧⌘S"),
            (RowAction::ApproveSpec, "⇧⌘A"),
        ] {
            let item = item(context(TaskState::Backlog), action).unwrap();
            assert_eq!(item.kbd.as_deref(), Some(shown), "{action:?}");
        }
    }

    /// A greyed item is still an item: the label a disabled verb shows names
    /// the verb first, so the menu reads the same whether or not it can run.
    #[test]
    fn the_verb_leads_even_when_it_cannot_run() {
        for context in every_context() {
            for item in items(context) {
                assert!(
                    item.menu_label().starts_with(item.label),
                    "{:?} in {context:?}",
                    item.menu_label()
                );
            }
        }
    }
}
