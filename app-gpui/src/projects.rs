//! The repo switcher's decisions, as pure functions over the lists the server
//! already returned.
//!
//! The switcher is a **filter over one working set**, held per-window — not a
//! scope, and not a server-side query parameter. `GET /tasks` is shared with
//! the orchestrator and `tasks status`, and a view
//! preference does not belong in it; it is the same argument the done-task
//! archive in [`crate::sections::tasks`] already makes, and the rows are a few
//! hundred at most.
//!
//! Everything decidable without a pixel lives here and is unit-tested. What is
//! left in `workspace.rs` is where the popover hangs and what it looks like,
//! which is `make app` on a Mac.

use tasks_client::api::models::{Project, ProjectId, ProjectStatus};

/// Which repo the window is looking at.
///
/// Per-window and resets on relaunch, like [`crate::workspace::Workspace`]'s
/// archive toggle: the app has no settings store, and a filter that names
/// itself in the title bar does not need one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProjectFilter {
    #[default]
    All,
    One(ProjectId),
}

impl ProjectFilter {
    /// Whether a row belonging to `project_id` is on screen.
    pub fn admits(&self, project_id: &ProjectId) -> bool {
        match self {
            ProjectFilter::All => true,
            ProjectFilter::One(selected) => selected == project_id,
        }
    }

    pub fn selected(&self) -> Option<&ProjectId> {
        match self {
            ProjectFilter::All => None,
            ProjectFilter::One(id) => Some(id),
        }
    }
}

/// What the rail header says the window is looking at, in the design's two
/// tones: an owner (muted) when the label names one repo, and the name at
/// full contrast. "All repos" carries no owner — it is a scope, not a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLabel {
    pub owner: Option<String>,
    pub name: String,
}

/// `None` before the first snapshot — a placeholder would be a claim about a
/// repo we have not read. With exactly one project configured it is that repo
/// whatever the filter says, which is what keeps a single-repo window reading
/// exactly as it did before multi-repo.
pub fn repo_label(projects: &[Project], filter: &ProjectFilter) -> Option<RepoLabel> {
    if projects.is_empty() {
        return None;
    }
    let named = |project: &Project| RepoLabel {
        owner: Some(project.repo_owner.clone()),
        name: project.repo_name.clone(),
    };
    match filter.selected() {
        Some(id) => projects.iter().find(|project| &project.id == id).map(named),
        // One repo is not a choice, so it is named rather than counted.
        None if projects.len() == 1 => Some(named(&projects[0])),
        None => Some(RepoLabel {
            owner: None,
            name: "All repos".to_string(),
        }),
    }
}

/// The switcher's rows: the server's order, with archived repos last.
///
/// Sorted last rather than dropped, deliberately — a repo you cannot select is
/// a repo you cannot un-archive, and archive is the only removal there is.
/// Stable within each half, so the server stays the authority on the order.
pub fn switcher_order(projects: &[Project]) -> Vec<&Project> {
    let (live, archived): (Vec<&Project>, Vec<&Project>) = projects
        .iter()
        .partition(|project| project.status != ProjectStatus::Archived);
    live.into_iter().chain(archived).collect()
}

/// Whether the rows *on screen* disagree about which repo they belong to.
///
/// The one question a row's repo label should be gated on. A single-repo
/// window — and a window filtered to one repo — is then pixel-identical to the
/// one before multi-repo, because naming the repo on every row of a list that
/// only has one repo in it is noise.
///
/// Over project ids rather than over rows, because the two callers hold
/// different row types (a `Task` in the Tasks list, a band row in the Queue)
/// and the question is the same one.
pub fn rows_are_ambiguous<'a>(project_ids: impl IntoIterator<Item = &'a ProjectId>) -> bool {
    let mut seen: Option<&ProjectId> = None;
    for id in project_ids {
        match seen {
            None => seen = Some(id),
            Some(first) if first != id => return true,
            Some(_) => {}
        }
    }
    false
}

/// How a row names its repo when the rows around it disagree: the repo name
/// alone, since the owner is the part that repeats.
///
/// `None` for a project this client has not seen — the row is still shown,
/// unlabelled, rather than being dropped for want of a name.
pub fn row_label(projects: &[Project], project_id: &ProjectId) -> Option<String> {
    projects
        .iter()
        .find(|project| &project.id == project_id)
        .map(|project| project.repo_name.clone())
}

/// The sentence a non-active repo carries in the switcher. `None` for `active`:
/// the normal case earns no badge.
///
/// **Not a second play/pause.** Mode is global and stays global — one
/// `SCOUT_MAX_CONCURRENT`, one strictly serial build lane, one vm-pool — so a
/// per-repo `play` could not run while another repo's build held the lane.
/// What is honestly per-repo is the subtraction, and these say which one.
pub fn status_note(status: ProjectStatus) -> Option<&'static str> {
    match status {
        ProjectStatus::Active => None,
        ProjectStatus::Paused => Some("Paused — issues still arrive, nothing is dispatched"),
        ProjectStatus::Archived => Some("Archived — no new issues, nothing is dispatched"),
    }
}

/// One entry of a repo's status menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusAction {
    pub label: &'static str,
    pub status: ProjectStatus,
    /// What it costs, in words, for the confirmation the human deserves before
    /// stopping a repository's whole pipeline.
    pub note: &'static str,
}

/// The transitions offered for a repo in this state.
///
/// There is no Delete, and its absence is the load-bearing part: `decisions` is
/// append-only and keyed to a project's tasks, and `tasks.project_id` is
/// `ON DELETE CASCADE`, so deleting a project would take the audit trail the
/// whole charter rests on with it. Archive *is* the removal.
///
/// Archived offers only the way back to active — pausing something already
/// archived is a distinction without a difference, and un-archive then pause is
/// two clicks nobody needs to be talked through.
pub fn status_actions(status: ProjectStatus) -> Vec<StatusAction> {
    const PAUSE: StatusAction = StatusAction {
        label: "Pause repo",
        status: ProjectStatus::Paused,
        note: "Stops new scouts and builds. Issues keep arriving; work in flight finishes.",
    };
    const ARCHIVE: StatusAction = StatusAction {
        label: "Archive repo",
        status: ProjectStatus::Archived,
        note: "Stops new issues as well. Nothing is deleted — the tasks and the ledger stay.",
    };
    const ACTIVATE: StatusAction = StatusAction {
        label: "Resume repo",
        status: ProjectStatus::Active,
        note: "Scouts and builds dispatch again.",
    };
    match status {
        ProjectStatus::Active => vec![PAUSE, ARCHIVE],
        ProjectStatus::Paused => vec![ACTIVATE, ARCHIVE],
        ProjectStatus::Archived => vec![ACTIVATE],
    }
}

/// Where a new issue would be filed, and whether it can be.
///
/// The server already refuses to guess between several projects; this is the
/// app deciding to stop asking the orchestrator to. The composer states the
/// answer and carries the id verbatim, so the agent copies a value rather than
/// re-deriving one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueTarget {
    /// File here. The id is what the message carries.
    Repo { id: ProjectId, slug: String },
    /// Several repos are in view and none is selected — pick one first. The
    /// same refusal the server makes, made before the message is sent rather
    /// than after.
    Ambiguous,
    /// The selected repo is archived: an issue filed there is never ingested,
    /// so it would vanish out of this app the moment it was created.
    Archived(String),
    /// Nothing is configured yet.
    NoRepo,
}

impl IssueTarget {
    /// Whether the composer's send is live.
    pub fn can_file(&self) -> bool {
        matches!(self, IssueTarget::Repo { .. })
    }

    /// The footer sentence — what the human reads before pressing send.
    pub fn sentence(&self) -> String {
        match self {
            IssueTarget::Repo { slug, .. } => format!("Files into {slug}"),
            IssueTarget::Ambiguous => {
                "Pick a repo in the title bar — several are in view".to_string()
            }
            IssueTarget::Archived(slug) => {
                format!("{slug} is archived — its issues are no longer ingested")
            }
            IssueTarget::NoRepo => "No repository is configured yet".to_string(),
        }
    }
}

/// The message a task-drafting surface hands the orchestrator, with the repo
/// pinned. One builder for every door into this flow (the ⌘N window, the
/// rail composer), so they cannot drift into asking the agent two different
/// things — the orchestrator owns titling and filing, the surface owns the
/// repo. `None` when the target cannot be filed into; the caller refuses
/// before anything is sent.
pub fn issue_prompt(target: &IssueTarget, draft: &str) -> Option<String> {
    let IssueTarget::Repo { id, slug } = target else {
        return None;
    };
    Some(format!(
        "Create a new GitHub issue from the draft below, in {slug}. Pass \
         \"project_id\": \"{id}\" to POST /issues — that is the repository \
         chosen in the app, not one to re-derive. Write a clear, specific \
         title, and expand the body with any relevant context you have \
         (related tasks, recent activity, code areas). File it and reply \
         with the issue number and link.\n\n\
         Draft:\n{draft}"
    ))
}

/// Resolve the composer's target from the projects and this window's filter.
///
/// The unfiltered single-repo case resolves rather than refusing, and matches
/// the server exactly: `resolve_project` answers "the only one there is" over
/// the **non-archived** projects.
pub fn issue_target(projects: &[Project], filter: &ProjectFilter) -> IssueTarget {
    if let Some(id) = filter.selected() {
        return match projects.iter().find(|project| &project.id == id) {
            Some(project) if project.status == ProjectStatus::Archived => {
                IssueTarget::Archived(project.slug())
            }
            Some(project) => IssueTarget::Repo {
                id: project.id.clone(),
                slug: project.slug(),
            },
            // The selected repo is gone from the snapshot: refuse rather than
            // silently widening to whatever is left.
            None => IssueTarget::NoRepo,
        };
    }
    let mut live = projects
        .iter()
        .filter(|project| project.status != ProjectStatus::Archived);
    match (live.next(), live.next()) {
        (Some(project), None) => IssueTarget::Repo {
            id: project.id.clone(),
            slug: project.slug(),
        },
        (Some(_), Some(_)) => IssueTarget::Ambiguous,
        _ => IssueTarget::NoRepo,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{GhState, Task, TaskId, TaskState};

    use super::*;

    fn project(name: &str, status: ProjectStatus) -> Project {
        Project {
            id: ProjectId::from_raw(format!("proj-{name}")),
            repo_owner: "iamnbutler".into(),
            repo_name: name.into(),
            added_at: Utc.timestamp_opt(0, 0).unwrap(),
            status,
        }
    }

    fn task(project: &Project, number: u64) -> Task {
        Task {
            id: TaskId::from_raw(format!("task-{number}")),
            project_id: project.id.clone(),
            gh_issue_number: number,
            title: format!("issue {number}"),
            body: String::new(),
            labels: Vec::new(),
            gh_state: GhState::Open,
            state: TaskState::Backlog,
            priority: 0,
            manual_rank: None,
            dispatch_attempts: 0,
            ingested_at: Utc.timestamp_opt(0, 0).unwrap(),
            updated_at: Utc.timestamp_opt(0, 0).unwrap(),
            scout_directions: None,
        }
    }

    /// Nothing renders before the first snapshot: a placeholder would be a
    /// claim about a repo we have not read.
    #[test]
    fn the_label_says_nothing_before_the_first_snapshot() {
        assert_eq!(repo_label(&[], &ProjectFilter::All), None);
    }

    /// One repo is not a choice — the rail header reads exactly as it did
    /// before there was a repo level.
    #[test]
    fn one_repo_is_named_rather_than_counted() {
        let projects = [project("tasks", ProjectStatus::Active)];
        assert_eq!(
            repo_label(&projects, &ProjectFilter::All),
            Some(RepoLabel {
                owner: Some("iamnbutler".to_string()),
                name: "tasks".to_string()
            })
        );
    }

    #[test]
    fn several_repos_unfiltered_read_as_all_repos_scope_not_name() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("gpuikit", ProjectStatus::Active),
        ];
        assert_eq!(
            repo_label(&projects, &ProjectFilter::All),
            Some(RepoLabel {
                owner: None,
                name: "All repos".to_string()
            })
        );
        let filter = ProjectFilter::One(projects[1].id.clone());
        assert_eq!(
            repo_label(&projects, &filter),
            Some(RepoLabel {
                owner: Some("iamnbutler".to_string()),
                name: "gpuikit".to_string()
            })
        );
    }

    /// Sorted last, not dropped — a repo you cannot select is a repo you
    /// cannot un-archive, and there is no other way back.
    #[test]
    fn archived_repos_sort_last_and_stay_listed() {
        let projects = [
            project("old", ProjectStatus::Archived),
            project("tasks", ProjectStatus::Active),
            project("paused", ProjectStatus::Paused),
        ];
        let order: Vec<&str> = switcher_order(&projects)
            .into_iter()
            .map(|project| project.repo_name.as_str())
            .collect();
        assert_eq!(order, ["tasks", "paused", "old"]);
    }

    /// Within each half the server's order stands: this partitions, it does
    /// not sort.
    #[test]
    fn switcher_order_is_stable_within_each_half() {
        let projects = [
            project("b", ProjectStatus::Active),
            project("a", ProjectStatus::Active),
            project("z", ProjectStatus::Archived),
            project("y", ProjectStatus::Archived),
        ];
        let order: Vec<&str> = switcher_order(&projects)
            .into_iter()
            .map(|project| project.repo_name.as_str())
            .collect();
        assert_eq!(order, ["b", "a", "z", "y"]);
    }

    #[test]
    fn rows_name_their_repo_only_when_the_rows_on_screen_disagree() {
        let one = project("tasks", ProjectStatus::Active);
        let two = project("gpuikit", ProjectStatus::Active);
        let (a, b, c) = (task(&one, 1), task(&one, 2), task(&two, 3));
        let ids = |tasks: &[&Task]| {
            rows_are_ambiguous(
                tasks
                    .iter()
                    .map(|task| &task.project_id)
                    .collect::<Vec<_>>(),
            )
        };

        assert!(!ids(&[]), "no rows, no ambiguity");
        assert!(!ids(&[&a]));
        assert!(
            !ids(&[&a, &b]),
            "two repos exist, but only one is on screen — the label would be noise"
        );
        assert!(ids(&[&a, &c]));
        assert!(ids(&[&c, &b]));
    }

    #[test]
    fn a_row_label_is_the_repo_name_and_survives_an_unknown_project() {
        let projects = [project("tasks", ProjectStatus::Active)];
        assert_eq!(
            row_label(&projects, &projects[0].id).as_deref(),
            Some("tasks")
        );
        assert_eq!(
            row_label(&projects, &ProjectId::from_raw("proj-gone")),
            None
        );
    }

    #[test]
    fn only_a_repo_that_is_subtracting_something_carries_a_note() {
        assert_eq!(status_note(ProjectStatus::Active), None);
        assert!(
            status_note(ProjectStatus::Paused)
                .unwrap()
                .contains("issues still arrive"),
            "paused says what still happens, or it reads as archived"
        );
        assert!(status_note(ProjectStatus::Archived)
            .unwrap()
            .contains("no new issues"));
    }

    /// There is no delete, at any status. Archive is the removal.
    #[test]
    fn no_status_ever_offers_a_delete() {
        for status in [
            ProjectStatus::Active,
            ProjectStatus::Paused,
            ProjectStatus::Archived,
        ] {
            let actions = status_actions(status);
            assert!(
                !actions
                    .iter()
                    .any(|a| a.label.to_lowercase().contains("delete")
                        || a.label.to_lowercase().contains("remove")),
                "{status} offered a delete"
            );
            assert!(
                !actions.iter().any(|a| a.status == status),
                "{status} offered a transition to itself"
            );
        }
    }

    #[test]
    fn archived_offers_only_the_way_back() {
        let actions = status_actions(ProjectStatus::Archived);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].status, ProjectStatus::Active);
    }

    #[test]
    fn active_and_paused_both_reach_every_other_state() {
        let from_active: Vec<ProjectStatus> = status_actions(ProjectStatus::Active)
            .into_iter()
            .map(|a| a.status)
            .collect();
        assert_eq!(
            from_active,
            [ProjectStatus::Paused, ProjectStatus::Archived]
        );
        let from_paused: Vec<ProjectStatus> = status_actions(ProjectStatus::Paused)
            .into_iter()
            .map(|a| a.status)
            .collect();
        assert_eq!(
            from_paused,
            [ProjectStatus::Active, ProjectStatus::Archived]
        );
    }

    /// The unfiltered single-repo case resolves, exactly as the server's
    /// `resolve_project` does — and for the same reason, over the non-archived
    /// projects only.
    #[test]
    fn one_live_repo_needs_no_choice() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("old", ProjectStatus::Archived),
        ];
        let target = issue_target(&projects, &ProjectFilter::All);
        assert_eq!(
            target,
            IssueTarget::Repo {
                id: projects[0].id.clone(),
                slug: "iamnbutler/tasks".into()
            }
        );
        assert!(target.can_file());
        assert_eq!(target.sentence(), "Files into iamnbutler/tasks");
    }

    /// The refusal the server makes, made before the message is sent rather
    /// than after — and never by picking one.
    #[test]
    fn several_live_repos_and_no_selection_refuses_rather_than_guessing() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("gpuikit", ProjectStatus::Paused),
        ];
        let target = issue_target(&projects, &ProjectFilter::All);
        assert_eq!(target, IssueTarget::Ambiguous);
        assert!(!target.can_file());

        // Selecting one answers it — including a paused one, which still
        // ingests.
        let filter = ProjectFilter::One(projects[1].id.clone());
        assert!(issue_target(&projects, &filter).can_file());
    }

    /// An issue filed into an archived repo is never ingested, so it would
    /// vanish out of this app the moment it was created.
    #[test]
    fn an_archived_repo_is_not_a_place_to_file() {
        let projects = [project("old", ProjectStatus::Archived)];
        let filter = ProjectFilter::One(projects[0].id.clone());
        let target = issue_target(&projects, &filter);
        assert_eq!(target, IssueTarget::Archived("iamnbutler/old".into()));
        assert!(!target.can_file());
        assert!(target.sentence().contains("archived"));
    }

    #[test]
    fn no_projects_is_not_a_place_to_file_either() {
        assert_eq!(issue_target(&[], &ProjectFilter::All), IssueTarget::NoRepo);
        // A selection the snapshot no longer holds refuses rather than
        // silently widening to whatever is left.
        let projects = [project("tasks", ProjectStatus::Active)];
        let filter = ProjectFilter::One(ProjectId::from_raw("proj-gone"));
        assert_eq!(issue_target(&projects, &filter), IssueTarget::NoRepo);
    }

    #[test]
    fn the_filter_admits_exactly_what_it_names() {
        let one = project("tasks", ProjectStatus::Active);
        let two = project("gpuikit", ProjectStatus::Active);
        assert!(ProjectFilter::All.admits(&one.id));
        assert!(ProjectFilter::All.admits(&two.id));
        let filter = ProjectFilter::One(one.id.clone());
        assert!(filter.admits(&one.id));
        assert!(!filter.admits(&two.id));
    }
}
