import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { ExternalLink, Check, X, Info } from "lucide-react";
import { toast } from "sonner";
import { useAppState } from "@/hooks/use-app-state";
import { flushMergeQueue, approveMerge, rejectMerge } from "@/lib/api";
import { formatRelativeTime, projectLabel } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { ListView, ListEmptyState } from "@/components/ui/list-view";
import { ListHeader, ListHeaderTabs } from "@/components/ui/list-header";
import {
  ListRow,
  LinkCell,
  TextCell,
  BadgeCell,
  TimeCell,
  IdCell,
  ProjectCell,
  ActionsCell,
} from "@/components/ui/list-row";
import {
  lifecyclePhases,
  type LifecyclePhase,
  statusBadge,
  prNumber,
  prRepo,
  prRepoShort,
  getTask,
} from "./columns";
import type { MergeQueueEntry, Project, Task } from "@/lib/types";

// ---------------------------------------------------------------------------
// Merge Queue Row
// ---------------------------------------------------------------------------

function MergeQueueRow({
  entry,
  tasks,
  projects,
  showProject,
  onRefresh,
}: {
  entry: MergeQueueEntry;
  tasks: Task[];
  projects: Project[];
  showProject: boolean;
  onRefresh: () => Promise<void>;
}) {
  const task = getTask(entry.task_id, tasks);
  const num = prNumber(entry.pr_url);

  // Get project name
  let projectName = task?.project ? projectLabel(task.project, projects) : null;
  if (!projectName) {
    projectName = prRepoShort(entry.pr_url) ?? "\u2014";
  }
  const shortProject = projectName.includes("/") ? projectName.split("/")[1] : projectName;

  // Get linked issue info
  let issueLink: { url: string; number: number } | null = null;
  if (task?.source.type === "github_issue") {
    const { source } = task;
    issueLink = {
      url: `https://github.com/${source.owner}/${source.repo}/issues/${source.number}`,
      number: source.number,
    };
  }

  async function handleApprove() {
    try {
      await approveMerge(entry.id);
      onRefresh();
    } catch {
      toast.error("Failed to approve merge entry");
    }
  }

  async function handleReject() {
    try {
      await rejectMerge(entry.id);
      onRefresh();
    } catch {
      toast.error("Failed to reject merge entry");
    }
  }

  return (
    <ListRow>
      {/* PR number */}
      {num ? (
        <LinkCell href={entry.pr_url} icon={<ExternalLink className="h-3 w-3" />} className="w-16">
          #{num}
        </LinkCell>
      ) : (
        <IdCell width="w-16">{"\u2014"}</IdCell>
      )}

      {/* Title */}
      <TextCell>
        {task ? (
          <Link
            to={`/tasks/${entry.task_id}`}
            className="hover:underline truncate block"
            onClick={(e) => e.stopPropagation()}
          >
            {task.title}
          </Link>
        ) : (
          <a
            href={entry.pr_url}
            target="_blank"
            rel="noopener noreferrer"
            className="text-muted-foreground hover:underline truncate block"
            onClick={(e) => e.stopPropagation()}
          >
            {prRepo(entry.pr_url)}#{num}
          </a>
        )}
        {entry.changes_requested_feedback && (
          <p className="text-xs text-muted-foreground truncate mt-0.5" title={entry.changes_requested_feedback}>
            {entry.changes_requested_feedback}
          </p>
        )}
      </TextCell>

      {/* Project */}
      {showProject && <ProjectCell>{shortProject}</ProjectCell>}

      {/* Status */}
      <BadgeCell>{statusBadge(entry.status)}</BadgeCell>

      {/* Queue position */}
      <IdCell width="w-12">
        {entry.queue_position !== undefined && entry.queue_position !== null
          ? `#${entry.queue_position}`
          : "\u2014"}
      </IdCell>

      {/* Linked issue */}
      {issueLink ? (
        <LinkCell href={issueLink.url} icon={<ExternalLink className="h-3 w-3" />} className="w-16">
          #{issueLink.number}
        </LinkCell>
      ) : (
        <IdCell width="w-16">{"\u2014"}</IdCell>
      )}

      {/* Queued time */}
      <TimeCell width="w-20">{formatRelativeTime(entry.queued_at)}</TimeCell>

      {/* Actions */}
      {entry.status === "pending" && (
        <ActionsCell>
          <div className="inline-flex items-center rounded-md border border-border">
            <Button
              variant="ghost"
              size="sm"
              className="h-7 gap-1 rounded-r-none border-r border-border"
              onClick={handleApprove}
            >
              <Check className="h-3.5 w-3.5" />
              Approve
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="h-7 gap-1 rounded-l-none"
              onClick={handleReject}
            >
              <X className="h-3.5 w-3.5" />
              Reject
            </Button>
          </div>
        </ActionsCell>
      )}
    </ListRow>
  );
}

// ---------------------------------------------------------------------------
// Merge List (with pagination)
// ---------------------------------------------------------------------------

function MergeList({
  entries,
  selectedProject,
  refreshSnapshot,
  snapshot,
}: {
  entries: MergeQueueEntry[];
  selectedProject: string | null;
  refreshSnapshot: () => Promise<void>;
  snapshot: NonNullable<ReturnType<typeof useAppState>["snapshot"]>;
}) {
  const [page, setPage] = useState(0);
  const pageSize = 50;

  const tasks = snapshot.tasks ?? [];
  const projects = snapshot.projects ?? [];
  const showProject = !selectedProject;

  // Simple pagination
  const totalPages = Math.ceil(entries.length / pageSize);
  const paginatedEntries = entries.slice(page * pageSize, (page + 1) * pageSize);

  if (entries.length === 0) {
    return (
      <ListEmptyState message="No entries in this phase." />
    );
  }

  return (
    <>
      <div>
        {paginatedEntries.map((entry) => (
          <MergeQueueRow
            key={entry.id}
            entry={entry}
            tasks={tasks}
            projects={projects}
            showProject={showProject}
            onRefresh={refreshSnapshot}
          />
        ))}
      </div>

      {totalPages > 1 && (
        <div className="flex items-center justify-between px-4 py-2 border-t">
          <span className="text-xs text-muted-foreground">
            Page {page + 1} of {totalPages}
          </span>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-xs"
              onClick={() => setPage((p) => Math.max(0, p - 1))}
              disabled={page === 0}
            >
              Previous
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-xs"
              onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
              disabled={page >= totalPages - 1}
            >
              Next
            </Button>
          </div>
        </div>
      )}
    </>
  );
}

export function MergeQueuePage() {
  const { snapshot, refreshSnapshot, filteredMergeQueue, selectedProject } = useAppState();
  const [flushing, setFlushing] = useState(false);
  const [activeTab, setActiveTab] = useState<LifecyclePhase>("review");

  const entries = filteredMergeQueue;

  // Group entries by lifecycle phase
  const groupedEntries = useMemo(() => {
    const groups: Record<LifecyclePhase, MergeQueueEntry[]> = {
      review: [],
      queue: [],
      completed: [],
    };

    for (const entry of entries) {
      const statuses = lifecyclePhases.review.statuses;
      if (statuses.includes(entry.status)) {
        groups.review.push(entry);
      } else if (lifecyclePhases.queue.statuses.includes(entry.status)) {
        groups.queue.push(entry);
      } else if (lifecyclePhases.completed.statuses.includes(entry.status)) {
        groups.completed.push(entry);
      }
    }

    return groups;
  }, [entries]);

  const approvedCount = groupedEntries.queue.length;
  const isPaused = snapshot?.mode === "pause";

  async function handleFlush() {
    setFlushing(true);
    try {
      await flushMergeQueue();
      await refreshSnapshot();
    } catch {
      toast.error("Failed to flush merge queue");
    } finally {
      setFlushing(false);
    }
  }

  if (!snapshot) {
    return (
      <ListEmptyState message="Loading..." />
    );
  }

  const tabsConfig = (Object.entries(lifecyclePhases) as [LifecyclePhase, typeof lifecyclePhases.review][]).map(
    ([phase, config]) => ({
      key: phase,
      label: config.label,
      count: groupedEntries[phase].length,
    })
  );

  const headerTabs = (
    <ListHeaderTabs
      tabs={tabsConfig}
      activeTab={activeTab}
      onTabChange={setActiveTab}
      variant="line"
      className="mt-1"
    />
  );

  const headerActions = isPaused && approvedCount > 0 ? (
    <Button
      size="sm"
      onClick={handleFlush}
      disabled={flushing}
      className="h-7 text-xs"
    >
      {flushing ? "Flushing..." : `Flush ${approvedCount} approved`}
    </Button>
  ) : undefined;

  return (
    <ListView
      header={
        <div className="border-b border-border">
          <ListHeader
            title="Merge Queue"
            count={entries.length}
            countLabel="entries"
            actions={headerActions}
          />
          {isPaused && (
            <div className="mx-4 mb-2 flex items-start gap-2.5 rounded-md border border-blue-500/30 bg-blue-500/10 px-3 py-2">
              <Info className="h-4 w-4 text-blue-400 mt-0.5 shrink-0" />
              <p className="text-xs text-muted-foreground">
                <span className="font-medium text-foreground">Pause mode:</span>{" "}
                Approved PRs are held for manual flush. Rejections, conflicts, and change requests continue to be processed automatically.
              </p>
            </div>
          )}
          <div className="px-4 pb-1">
            {headerTabs}
          </div>
        </div>
      }
    >
      <MergeList
        entries={groupedEntries[activeTab]}
        selectedProject={selectedProject}
        refreshSnapshot={refreshSnapshot}
        snapshot={snapshot}
      />
    </ListView>
  );
}
