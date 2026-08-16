//! The repo switcher's decisions, as pure functions over the lists the server
//! already returned.
//!
//! Everything here is decidable without a pixel, and is therefore unit-tested
//! rather than looked at: which repo a window is showing, what the title bar
//! calls it, what order the switcher's rows come in, whether a list has to name
//! each row's repo, and which project a new issue is filed into. What is left
//! for `make app` on a Mac is where the popover lands and how the rows read at
//! 240px.
//!
//! The filter is a **client-side view filter** over one working set, not a
//! server-side query parameter and not a scope. `GET /tasks` is shared with the
//! orchestrator, the briefing generator and `tasks status`, and a view
//! preference does not belong in it — the same argument the done-task archive
//! in [`crate::sections::tasks`] already makes.

use tasks_client::api::models::{Project, ProjectId, ProjectStatus, Task};

/// Which repo a window is showing.
///
/// Per-window and not persisted: the app has no settings store, and a filter
/// whose current state is written across the title bar does not need one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProjectFilter {
    #[default]
    All,
    One(ProjectId),
}

impl ProjectFilter {
    pub fn selected(&self) -> Option<&ProjectId> {
        match self {
            ProjectFilter::All => None,
            ProjectFilter::One(id) => Some(id),
        }
    }

    /// Whether a task belongs to the repo on screen.
    pub fn admits(&self, task: &Task) -> bool {
        match self {
            ProjectFilter::All => true,
            ProjectFilter::One(id) => &task.project_id == id,
        }
    }

    /// The filter applied to a list of tasks. Drops rows, never sorts them —
    /// the server orders the working set and this only narrows it.
    pub fn apply<'a>(&self, tasks: &'a [Task]) -> Vec<&'a Task> {
        tasks.iter().filter(|task| self.admits(task)).collect()
    }
}

/// What the title bar's switcher says.
///
/// Nothing at all before the first snapshot: a placeholder would be a claim
/// about a repo we have not read. With one repo tracked it names that repo and
/// never "All repos" — a chooser between one thing is a label, and this is the
/// pixel-identical case that keeps a single-repo window looking like the one
/// before multi-repo.
pub fn switcher_label(projects: &[Project], filter: &ProjectFilter) -> Option<String> {
    match filter {
        ProjectFilter::One(id) => projects
            .iter()
            .find(|project| &project.id == id)
            .map(Project::slug),
        ProjectFilter::All => match projects {
            [] => None,
            [only] => Some(only.slug()),
            _ => Some("All repos".to_string()),
        },
    }
}

/// The switcher's rows: active and paused repos in the server's order, then
/// archived ones.
///
/// Archived are sorted last rather than dropped. `GET /projects` returns them
/// deliberately, and a repo you cannot select is a repo you cannot un-archive.
pub fn switcher_order(projects: &[Project]) -> Vec<&Project> {
    let mut rows: Vec<&Project> = projects.iter().collect();
    rows.sort_by_key(|project| project.status == ProjectStatus::Archived);
    rows
}

/// One switcher row's label: the slug, plus its status when that status is
/// news. `active` is the default and says nothing.
pub fn row_label(project: &Project) -> String {
    match project.status {
        ProjectStatus::Active => project.slug(),
        ProjectStatus::Paused => format!("{} · paused", project.slug()),
        ProjectStatus::Archived => format!("{} · archived", project.slug()),
    }
}

/// Whether the rows *on screen* disagree about which repo they belong to.
///
/// The one thing that decides whether a task or queue row spells its repo out.
/// A single-repo window — and a window filtered to one repo — is pixel-
/// identical to the one before multi-repo, because there is nothing to
/// disambiguate; the moment two repos share a list, every row says which.
pub fn rows_are_ambiguous(tasks: &[&Task]) -> bool {
    let mut seen: Option<&ProjectId> = None;
    for task in tasks {
        match seen {
            None => seen = Some(&task.project_id),
            Some(first) if first != &task.project_id => return true,
            Some(_) => {}
        }
    }
    false
}

/// The short name a row uses when it has to name its repo: the repository, not
/// the owner. Rows are narrow and the owner is the half that repeats.
pub fn row_repo_name(projects: &[Project], task: &Task) -> Option<String> {
    projects
        .iter()
        .find(|project| project.id == task.project_id)
        .map(|project| project.repo_name.clone())
}

/// The sentence the switcher shows under a repo that is not active — the whole
/// of what pausing and archiving mean, in the place the human chose them.
pub fn status_note(status: ProjectStatus) -> Option<&'static str> {
    match status {
        ProjectStatus::Active => None,
        ProjectStatus::Paused => {
            Some("Paused — issues are still ingested, but no scout or build starts.")
        }
        ProjectStatus::Archived => {
            Some("Archived — no new issues, no new work. Open pull requests are still watched.")
        }
    }
}

/// The status verbs offered for a repo: every value it is not currently in.
///
/// A radio group rather than a toggle, because the three values are ordered
/// and "unpause" from archived would have to guess which of the two remaining
/// states the human meant.
pub fn status_actions(status: ProjectStatus) -> Vec<(ProjectStatus, &'static str)> {
    ProjectStatus::ALL
        .into_iter()
        .filter(|candidate| *candidate != status)
        .map(|candidate| {
            let label = match candidate {
                ProjectStatus::Active => "Resume",
                ProjectStatus::Paused => "Pause",
                ProjectStatus::Archived => "Archive",
            };
            (candidate, label)
        })
        .collect()
}

/// Which project a new issue is filed into, and why not.
///
/// The server already refuses to guess between several; this is the app
/// deciding to stop *asking* the orchestrator to. An archived repo is not a
/// candidate for new work, which mirrors `resolve_project` on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueTarget {
    /// File into this repo, named for the human and carried as an id in the
    /// message so the agent copies a value rather than re-deriving one.
    Repo { id: ProjectId, slug: String },
    /// Nothing to file into.
    NoProjects,
    /// Several are in view and none is selected — the one case where the
    /// composer refuses to send.
    Ambiguous { count: usize },
}

pub fn issue_target(projects: &[Project], filter: &ProjectFilter) -> IssueTarget {
    let named = filter
        .selected()
        .and_then(|id| projects.iter().find(|project| &project.id == id));
    if let Some(project) = named {
        return IssueTarget::Repo {
            id: project.id.clone(),
            slug: project.slug(),
        };
    }
    let mut candidates = projects
        .iter()
        .filter(|project| project.status != ProjectStatus::Archived);
    match (candidates.next(), candidates.next()) {
        (None, _) => IssueTarget::NoProjects,
        (Some(only), None) => IssueTarget::Repo {
            id: only.id.clone(),
            slug: only.slug(),
        },
        (Some(_), Some(_)) => IssueTarget::Ambiguous {
            count: projects
                .iter()
                .filter(|project| project.status != ProjectStatus::Archived)
                .count(),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{GhState, TaskId, TaskState};

    use super::*;

    fn project(name: &str, status: ProjectStatus) -> Project {
        Project {
            id: ProjectId::from_raw(format!("proj-{name}")),
            repo_owner: "iamnbutler".into(),
            repo_name: name.into(),
            status,
            added_at: Utc.timestamp_opt(0, 0).unwrap(),
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
        }
    }

    #[test]
    fn nothing_is_claimed_before_the_first_snapshot() {
        assert_eq!(switcher_label(&[], &ProjectFilter::All), None);
    }

    /// One repo is a label, not a chooser — the case that keeps a single-repo
    /// window reading exactly as it did before multi-repo.
    #[test]
    fn one_repo_names_itself_rather_than_all_repos() {
        let only = project("tasks", ProjectStatus::Active);
        assert_eq!(
            switcher_label(&[only], &ProjectFilter::All).as_deref(),
            Some("iamnbutler/tasks")
        );
    }

    #[test]
    fn several_repos_unfiltered_read_as_all_repos() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("gpuikit", ProjectStatus::Active),
        ];
        assert_eq!(
            switcher_label(&projects, &ProjectFilter::All).as_deref(),
            Some("All repos")
        );
    }

    #[test]
    fn a_selected_repo_names_itself() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("gpuikit", ProjectStatus::Active),
        ];
        let filter = ProjectFilter::One(projects[1].id.clone());
        assert_eq!(
            switcher_label(&projects, &filter).as_deref(),
            Some("iamnbutler/gpuikit")
        );
    }

    /// A filter pointing at a project the snapshot no longer has says nothing
    /// rather than inventing a name.
    #[test]
    fn a_filter_on_a_vanished_repo_names_nothing() {
        let projects = [project("tasks", ProjectStatus::Active)];
        let filter = ProjectFilter::One(ProjectId::from_raw("proj-gone"));
        assert_eq!(switcher_label(&projects, &filter), None);
    }

    /// Archived repos sort last and are still listed — a repo you cannot
    /// select is a repo you cannot un-archive.
    #[test]
    fn archived_repos_sort_last_and_are_never_dropped() {
        let projects = [
            project("old", ProjectStatus::Archived),
            project("tasks", ProjectStatus::Active),
            project("paused", ProjectStatus::Paused),
        ];
        let order: Vec<_> = switcher_order(&projects)
            .into_iter()
            .map(|project| project.repo_name.clone())
            .collect();
        assert_eq!(order, ["tasks", "paused", "old"]);
    }

    #[test]
    fn a_row_states_a_status_only_when_it_is_news() {
        assert_eq!(
            row_label(&project("tasks", ProjectStatus::Active)),
            "iamnbutler/tasks"
        );
        assert_eq!(
            row_label(&project("tasks", ProjectStatus::Paused)),
            "iamnbutler/tasks · paused"
        );
    }

    #[test]
    fn the_filter_narrows_without_reordering() {
        let a = project("tasks", ProjectStatus::Active);
        let b = project("gpuikit", ProjectStatus::Active);
        let tasks = [task(&a, 1), task(&b, 2), task(&a, 3)];
        let filter = ProjectFilter::One(a.id.clone());
        let numbers: Vec<_> = filter
            .apply(&tasks)
            .into_iter()
            .map(|task| task.gh_issue_number)
            .collect();
        assert_eq!(numbers, [1, 3]);
        assert_eq!(ProjectFilter::All.apply(&tasks).len(), 3);
    }

    /// Rows name their repo only when the rows on screen disagree about it.
    #[test]
    fn rows_name_their_repo_only_when_they_disagree() {
        let a = project("tasks", ProjectStatus::Active);
        let b = project("gpuikit", ProjectStatus::Active);
        let one_repo = [task(&a, 1), task(&a, 2)];
        let two_repos = [task(&a, 1), task(&b, 2)];
        assert!(!rows_are_ambiguous(&one_repo.iter().collect::<Vec<_>>()));
        assert!(rows_are_ambiguous(&two_repos.iter().collect::<Vec<_>>()));
        assert!(!rows_are_ambiguous(&[]));
    }

    /// …and a *filtered* list is unambiguous by construction, which is what
    /// keeps a window narrowed to one repo pixel-identical to a single-repo
    /// one.
    #[test]
    fn filtering_to_one_repo_removes_the_ambiguity_it_was_naming() {
        let a = project("tasks", ProjectStatus::Active);
        let b = project("gpuikit", ProjectStatus::Active);
        let tasks = [task(&a, 1), task(&b, 2)];
        let filter = ProjectFilter::One(a.id.clone());
        assert!(!rows_are_ambiguous(&filter.apply(&tasks)));
    }

    #[test]
    fn a_repo_offers_every_status_it_is_not_in() {
        let verbs: Vec<_> = status_actions(ProjectStatus::Active)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(verbs, ["Pause", "Archive"]);
        let verbs: Vec<_> = status_actions(ProjectStatus::Archived)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(verbs, ["Resume", "Pause"]);
    }

    #[test]
    fn only_a_non_active_repo_says_what_its_status_means() {
        assert!(status_note(ProjectStatus::Active).is_none());
        assert!(status_note(ProjectStatus::Paused).is_some());
        assert!(status_note(ProjectStatus::Archived).is_some());
    }

    /// The composer states the target rather than asking the orchestrator to
    /// pick, and the selected repo is that target whatever else is tracked.
    #[test]
    fn the_selected_repo_is_the_issue_target() {
        let a = project("tasks", ProjectStatus::Active);
        let b = project("gpuikit", ProjectStatus::Active);
        let projects = [a.clone(), b];
        let target = issue_target(&projects, &ProjectFilter::One(a.id.clone()));
        assert_eq!(
            target,
            IssueTarget::Repo {
                id: a.id,
                slug: "iamnbutler/tasks".into()
            }
        );
    }

    #[test]
    fn one_live_repo_needs_no_selection() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("old", ProjectStatus::Archived),
        ];
        assert!(matches!(
            issue_target(&projects, &ProjectFilter::All),
            IssueTarget::Repo { .. }
        ));
    }

    /// Several in view and none selected is the one case the composer refuses:
    /// the server refuses to guess, and so does this.
    #[test]
    fn several_repos_and_no_selection_refuses_rather_than_guessing() {
        let projects = [
            project("tasks", ProjectStatus::Active),
            project("gpuikit", ProjectStatus::Paused),
        ];
        assert_eq!(
            issue_target(&projects, &ProjectFilter::All),
            IssueTarget::Ambiguous { count: 2 }
        );
    }

    /// Naming an archived repo still targets it — the same asymmetry
    /// `resolve_project` has on the server: archived is not a candidate for
    /// *guessing*, but it is still addressable.
    #[test]
    fn an_archived_repo_is_still_addressable_when_named() {
        let archived = project("old", ProjectStatus::Archived);
        let projects = [archived.clone()];
        assert!(matches!(
            issue_target(&projects, &ProjectFilter::One(archived.id.clone())),
            IssueTarget::Repo { .. }
        ));
        assert_eq!(
            issue_target(&projects, &ProjectFilter::All),
            IssueTarget::NoProjects
        );
    }

    #[test]
    fn no_projects_at_all_is_its_own_answer() {
        assert_eq!(
            issue_target(&[], &ProjectFilter::All),
            IssueTarget::NoProjects
        );
    }
}
