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
//! banner. The predicates below are the store's: `queue_task` takes `Backlog`,
//! or `Rejected` while the issue is still open (#1028 — a task rejected by
//! attrition rather than by verdict), `dequeue_task` requires `Queued`,
//! `push_task_to_front` takes either, `create_build` takes only an approved
//! spec, and a review verdict needs a `pending_review` entry.

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
        TaskState::AwaitingMerge => Some("its pull request is open"),
        TaskState::Done => Some("this task is done"),
        // `Rejected` names two outcomes and the server admits only one of them
        // back (#1028): a task rejected by *attrition* — three scouts, no spec
        // — still has an open issue and is re-queueable, while one rejected by
        // *verdict* has a closed issue and is not. Mirrored from
        // `Store::queue_task` rather than invented here; enabling the item for
        // a closed issue would produce a button that 400s.
        TaskState::Rejected if context.gh_state == GhState::Open => None,
        TaskState::Rejected => Some("this task was rejected and its issue is closed"),
    }
}

fn dequeue_refusal(context: RowContext) -> Option<&'static str> {
    match context.task_state {
        TaskState::Queued => None,
        TaskState::Backlog => Some("not queued"),
        TaskState::Scouting => Some("a scout is running"),
        // Work past `Queued` cannot be un-picked: it stays picked up, and a
        // spec's fate is decided by reviewing it.
        TaskState::InReview
        | TaskState::ReadyToBuild
        | TaskState::Building
        | TaskState::AwaitingMerge => Some("work has already started"),
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
        TaskState::AwaitingMerge => Some("its pull request is open"),
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
        // `AwaitingMerge` is past the run, not in it: the build concluded and
        // opened a PR, so there is nothing here to stop.
        TaskState::InReview | TaskState::ReadyToBuild | TaskState::AwaitingMerge => {
            Some("nothing is running")
        }
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

// --- the bulk half: the same derivation, folded over a selection ---

/// The verbs the All Tasks selection bar offers, in the order it offers them.
///
/// A fixed ordered **subset** of [`entries`], not a second list of verbs: the
/// legality of each still comes from `entries`, so there is exactly one
/// derivation of "can this run?" in this module and both surfaces read it.
///
/// What is deliberately absent is the sharper half of the change. The three
/// destructive verbs ([`RowAction::CancelRun`], [`RowAction::CloseCompleted`],
/// [`RowAction::CloseNotPlanned`]) are left off because the single-row menu's
/// no-confirmation argument — ledger plus recourse, not pre-approval — is
/// about one misclick with one undo directly beneath it, and it does not
/// survive multiplication: twelve closes is twelve undos, and a cancelled
/// run's VM hours no undo returns. [`RowAction::ReviewSpec`] is absent because
/// there is one composer and one spec, with no N-shaped place for a verdict to
/// land, and [`RowAction::OpenOnGitHub`] because N browser tabs from one click
/// is not a verb anyone asked for.
pub const BULK_ACTIONS: [RowAction; 6] = [
    RowAction::Queue,
    RowAction::Dequeue,
    RowAction::ScoutNow,
    RowAction::ApproveSpec,
    RowAction::RequestBuild,
    RowAction::Reopen,
];

/// One bulk verb as the Actions menu will render it: how much of the
/// selection it can run on, and — when some of it cannot — why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkItem {
    pub action: RowAction,
    pub id: &'static str,
    pub label: &'static str,
    /// How many of the selected rows admit this verb.
    pub eligible: usize,
    /// How many rows were considered.
    pub total: usize,
    /// Why the rest cannot, aggregated by reason in first-seen order. The
    /// refusals are already `&'static str`, so aggregating them is free.
    pub refusals: Vec<&'static str>,
}

impl BulkItem {
    /// Nothing in the selection admits this verb.
    pub fn is_disabled(&self) -> bool {
        self.eligible == 0
    }

    /// The label as the menu renders it: the verb with its reach, or — when
    /// nothing is eligible — the reason, in the same shape a greyed row item
    /// uses.
    pub fn menu_label(&self) -> String {
        if self.is_disabled() {
            return match self.refusals.first() {
                Some(reason) => format!("{} ({reason})", self.label),
                // Only reachable with an empty selection, where the bar is not
                // on screen at all.
                None => format!("{} (nothing selected)", self.label),
            };
        }
        if self.eligible == self.total {
            return self.label.to_string();
        }
        format!("{} ({} of {})", self.label, self.eligible, self.total)
    }

    /// What the banner says once the verb has run. A different job from
    /// [`Self::menu_label`]: that one is a forecast, this one is a receipt,
    /// and a receipt has to account for the rows that were skipped.
    pub fn receipt(&self) -> String {
        let skipped = self.total - self.eligible;
        if skipped == 0 {
            return format!("{}: {}", self.label, self.eligible);
        }
        format!(
            "{}: {} · {skipped} skipped ({})",
            self.label,
            self.eligible,
            self.refusals.join(", ")
        )
    }
}

/// Fold [`entries`] over a selection: for each bulk verb, how many of these
/// rows admit it and why the rest do not.
///
/// O(rows x verbs), called on every open of the Actions menu and every render
/// of the bar's count — cheap over a few hundred rows, and it must not grow a
/// server round trip.
pub fn bulk_entries(selection: &[RowContext]) -> Vec<BulkItem> {
    BULK_ACTIONS
        .into_iter()
        .map(|action| {
            let mut eligible = 0;
            let mut refusals: Vec<&'static str> = Vec::new();
            let mut label = "";
            let mut id = "";
            for context in selection {
                let Some(row) = item(*context, action) else {
                    continue;
                };
                label = row.label;
                id = row.id;
                match row.disabled {
                    None => eligible += 1,
                    Some(reason) => {
                        if !refusals.contains(&reason) {
                            refusals.push(reason);
                        }
                    }
                }
            }
            // An empty selection still has to name its verb: the labels are
            // fixed strings on the entry list, so read them off a context that
            // exists rather than leaving the item blank.
            if label.is_empty() {
                let fallback = item(
                    RowContext {
                        task_state: TaskState::Backlog,
                        gh_state: GhState::Open,
                        has_github_url: true,
                        spec: None,
                    },
                    action,
                )
                .expect("every bulk action is a row verb");
                label = fallback.label;
                id = fallback.id;
            }
            BulkItem {
                action,
                id,
                label,
                eligible,
                total: selection.len(),
                refusals,
            }
        })
        .collect()
}

/// The bulk item for `action`, or `None` when the bar does not offer it.
pub fn bulk_item(selection: &[RowContext], action: RowAction) -> Option<BulkItem> {
    bulk_entries(selection)
        .into_iter()
        .find(|item| item.action == action)
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

    /// #1028: the server admits a task rejected by *attrition* back into the
    /// queue while its issue is still open, so the menu must offer it. This
    /// mirrors `Store::queue_task` and is the whole reason those rows are kept
    /// on screen — `list_active_tasks` calls them the "close the issue or
    /// re-queue?" decision surface, and until the server grew the second arm
    /// the menu could only ever grey it out.
    #[test]
    fn a_rejected_task_with_an_open_issue_can_be_queued_again() {
        let rejected_open = RowContext {
            gh_state: GhState::Open,
            ..context(TaskState::Rejected)
        };
        assert_eq!(refusal(rejected_open, RowAction::Queue), None);
        assert!(enabled(rejected_open).contains(&RowAction::Queue));
    }

    /// The other kind: a closed issue means the rejection was a verdict, the
    /// server refuses it, and offering the verb would produce a button that
    /// 400s. The refusal also has to say *which* rejection this is, or it reads
    /// as the old blanket one.
    #[test]
    fn a_rejected_task_with_a_closed_issue_still_cannot_be_queued() {
        let rejected_closed = RowContext {
            gh_state: GhState::Closed,
            ..context(TaskState::Rejected)
        };
        let reason = refusal(rejected_closed, RowAction::Queue).expect("still refused");
        assert!(reason.contains("issue is closed"), "{reason}");
        assert!(!enabled(rejected_closed).contains(&RowAction::Queue));
    }

    /// Why `action` cannot run on this row, or `None` when it can.
    fn refusal(context: RowContext, action: RowAction) -> Option<&'static str> {
        item(context, action).and_then(|item| item.disabled)
    }

    /// Every combination of row state there is — 9 task states x 2 GitHub
    /// states x 2 project-known x 7 spec statuses (including none).
    fn every_context() -> Vec<RowContext> {
        let states = [
            TaskState::Backlog,
            TaskState::Queued,
            TaskState::Scouting,
            TaskState::InReview,
            TaskState::ReadyToBuild,
            TaskState::Building,
            TaskState::AwaitingMerge,
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

#[cfg(test)]
mod bulk_tests {
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

    fn bulk(selection: &[RowContext], action: RowAction) -> BulkItem {
        bulk_item(selection, action).expect("the bar offers this verb")
    }

    /// The bar's verbs are a subset of the row menu's, in the row menu's own
    /// order — one list of verbs, not two.
    #[test]
    fn the_bar_offers_a_subset_of_the_row_menu_in_its_order() {
        let all: Vec<RowAction> = entries(context(TaskState::Backlog))
            .into_iter()
            .filter_map(|entry| match entry {
                RowEntry::Item(item) => Some(item.action),
                RowEntry::Separator => None,
            })
            .collect();
        let bar: Vec<RowAction> = BULK_ACTIONS.into_iter().collect();
        for action in &bar {
            assert!(all.contains(action), "{action:?} is not a row verb");
        }
        let positions: Vec<usize> = bar
            .iter()
            .map(|action| all.iter().position(|other| other == action).unwrap())
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "the bar reorders the row menu");
    }

    /// The sharper half of this change, and the property that has to survive
    /// somebody adding a verb later: nothing the bar offers is destructive.
    /// The single-row menu's no-confirmation argument is about one misclick
    /// with one undo beneath it, and it does not survive multiplication.
    #[test]
    fn every_offered_verb_is_non_destructive() {
        // Read off `entries` rather than restated, so a verb that *becomes*
        // destructive later fails here too.
        let destructive: Vec<RowAction> = entries(context(TaskState::Scouting))
            .into_iter()
            .filter_map(|entry| match entry {
                RowEntry::Item(item) if item.destructive => Some(item.action),
                _ => None,
            })
            .collect();
        for action in BULK_ACTIONS {
            assert!(
                !destructive.contains(&action),
                "{action:?} is destructive and must not be offered in bulk"
            );
        }
    }

    /// Named individually as well, because the exclusions *are* the
    /// deliverable: these five are decisions, not an abbreviation.
    #[test]
    fn the_named_exclusions_stay_excluded() {
        for action in [
            RowAction::CancelRun,
            RowAction::CloseCompleted,
            RowAction::CloseNotPlanned,
            RowAction::ReviewSpec,
            RowAction::OpenOnGitHub,
        ] {
            assert!(
                !BULK_ACTIONS.contains(&action),
                "{action:?} must not reach the bulk bar"
            );
            assert!(bulk_item(&[context(TaskState::Backlog)], action).is_none());
        }
    }

    /// Partial legality is the normal case, and the count is what says so.
    #[test]
    fn a_verb_counts_the_rows_that_admit_it() {
        let selection = [
            context(TaskState::Backlog),
            context(TaskState::Backlog),
            context(TaskState::Queued),
        ];
        let queue = bulk(&selection, RowAction::Queue);
        assert_eq!((queue.eligible, queue.total), (2, 3));
        assert_eq!(queue.menu_label(), "Add to Queue (2 of 3)");
        assert!(!queue.is_disabled());
    }

    /// A verb the whole selection admits does not shout a fraction at you.
    #[test]
    fn a_wholly_eligible_verb_reads_as_the_plain_verb() {
        let selection = [context(TaskState::Backlog), context(TaskState::Backlog)];
        assert_eq!(
            bulk(&selection, RowAction::Queue).menu_label(),
            "Add to Queue"
        );
    }

    /// Nothing eligible is greyed with a reason, exactly as a row item is —
    /// a dead item with no answer is what this shape exists to prevent.
    #[test]
    fn a_verb_nothing_admits_says_why() {
        let selection = [context(TaskState::Done), context(TaskState::Done)];
        let queue = bulk(&selection, RowAction::Queue);
        assert!(queue.is_disabled());
        assert_eq!(queue.menu_label(), "Add to Queue (this task is done)");
    }

    /// Refusals aggregate by reason, in first-seen order, without repeats.
    #[test]
    fn refusals_aggregate_by_reason_in_first_seen_order() {
        let selection = [
            context(TaskState::Scouting),
            context(TaskState::Done),
            context(TaskState::Scouting),
        ];
        let queue = bulk(&selection, RowAction::Queue);
        assert_eq!(queue.refusals, ["a scout is running", "this task is done"]);
    }

    /// The receipt is a different sentence from the forecast: it accounts for
    /// what was skipped and why.
    #[test]
    fn the_receipt_accounts_for_the_skipped_rows() {
        let selection = [
            context(TaskState::Backlog),
            context(TaskState::Backlog),
            context(TaskState::Backlog),
            context(TaskState::Queued),
            context(TaskState::Done),
        ];
        let queue = bulk(&selection, RowAction::Queue);
        assert_eq!(
            queue.receipt(),
            "Add to Queue: 3 · 2 skipped (already queued, this task is done)"
        );
    }

    /// Nothing skipped, nothing to explain.
    #[test]
    fn a_clean_run_reports_only_the_count() {
        let selection = [context(TaskState::Backlog), context(TaskState::Backlog)];
        assert_eq!(
            bulk(&selection, RowAction::Queue).receipt(),
            "Add to Queue: 2"
        );
    }

    /// The legality is `entries`' and not a second opinion: every bulk item's
    /// eligible count is exactly the number of rows the row menu would enable.
    #[test]
    fn eligibility_is_the_row_menus_own() {
        let selection = [
            context(TaskState::Backlog),
            context(TaskState::Queued),
            with_spec(TaskState::InReview, SpecQueueStatus::PendingReview),
            with_spec(TaskState::ReadyToBuild, SpecQueueStatus::Approved),
            context(TaskState::Done),
        ];
        for bulk in bulk_entries(&selection) {
            let expected = selection
                .iter()
                .filter(|context| {
                    item(**context, bulk.action).is_some_and(|row| row.disabled.is_none())
                })
                .count();
            assert_eq!(bulk.eligible, expected, "{:?}", bulk.action);
        }
    }

    /// An empty selection still names its verbs — the bar is not on screen
    /// then, but a menu built from a stale selection must not render blanks.
    #[test]
    fn an_empty_selection_still_names_every_verb() {
        for bulk in bulk_entries(&[]) {
            assert!(!bulk.label.is_empty(), "{:?}", bulk.action);
            assert!(!bulk.id.is_empty(), "{:?}", bulk.action);
            assert!(bulk.is_disabled());
            assert_eq!(
                bulk.menu_label(),
                format!("{} (nothing selected)", bulk.label)
            );
        }
    }

    /// Reopen is never greyed on a row, so it is never greyed in bulk either.
    #[test]
    fn reopen_is_eligible_for_every_selection() {
        let selection = [
            context(TaskState::Done),
            context(TaskState::Rejected),
            context(TaskState::Scouting),
        ];
        let reopen = bulk(&selection, RowAction::Reopen);
        assert_eq!(reopen.eligible, 3);
        assert!(reopen.refusals.is_empty());
    }

    /// The labels are the row menu's own, so the two surfaces cannot drift
    /// into calling one verb two things.
    #[test]
    fn the_labels_are_the_row_menus() {
        let selection = [context(TaskState::Backlog)];
        for bulk in bulk_entries(&selection) {
            let row = item(context(TaskState::Backlog), bulk.action).unwrap();
            assert_eq!(bulk.label, row.label);
            assert_eq!(bulk.id, row.id);
        }
    }
}
