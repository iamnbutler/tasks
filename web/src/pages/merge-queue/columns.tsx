import { useState } from "react";
import { type ColumnDef } from "@tanstack/react-table";
import { Link } from "react-router-dom";
import { ExternalLink, Check, X, MessageSquare } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
  DialogClose,
} from "@/components/ui/dialog";
import { Textarea } from "@/components/ui/textarea";
import { formatRelativeTime, projectLabel } from "@/lib/utils";
import { approveMerge, rejectMerge, requestChanges } from "@/lib/api";
import type { MergeQueueEntry, MergeStatus, Project, Task } from "@/lib/types";

// ---------------------------------------------------------------------------
// Status badge
// ---------------------------------------------------------------------------

export function statusBadge(status: MergeStatus) {
  switch (status) {
    case "pending":
      return <Badge variant="outline" className="bg-yellow-500/15 text-yellow-400 border-yellow-500/30">{status}</Badge>;
    case "approved":
      return <Badge variant="outline" className="bg-green-500/15 text-green-400 border-green-500/30">{status}</Badge>;
    case "merging":
      return <Badge variant="outline" className="bg-cyan-500/15 text-cyan-400 border-cyan-500/30 animate-pulse">{status}</Badge>;
    case "rejected":
      return <Badge variant="outline" className="bg-red-500/15 text-red-400 border-red-500/30">{status}</Badge>;
    case "merged":
      return <Badge variant="outline" className="bg-blue-500/15 text-blue-400 border-blue-500/30">{status}</Badge>;
    case "conflict":
      return <Badge variant="outline" className="bg-orange-500/15 text-orange-400 border-orange-500/30">{status}</Badge>;
    case "changes_requested":
      return <Badge variant="outline" className="bg-amber-500/15 text-amber-400 border-amber-500/30">changes requested</Badge>;
    default:
      return <Badge variant="outline">{status}</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Status sort order (pending last)
// ---------------------------------------------------------------------------

const statusSortOrder: Record<MergeStatus, number> = {
  changes_requested: 0, // Changes requested shown first (needs attention)
  conflict: 1,
  merging: 2, // Actively merging — shows current operation
  approved: 3,
  rejected: 4,
  merged: 5,
  pending: 6,
};

export { statusSortOrder };

// ---------------------------------------------------------------------------
// Lifecycle phase grouping
// ---------------------------------------------------------------------------

export type LifecyclePhase = "review" | "queue" | "completed";

export const lifecyclePhases: Record<LifecyclePhase, { label: string; statuses: MergeStatus[] }> = {
  review: {
    label: "Needs Review",
    statuses: ["changes_requested", "conflict", "pending"],
  },
  queue: {
    label: "Ready to Merge",
    statuses: ["approved"],
  },
  completed: {
    label: "Completed",
    statuses: ["merged", "rejected"],
  },
};

/** Get the lifecycle phase for a given status */
export function getLifecyclePhase(status: MergeStatus): LifecyclePhase {
  for (const [phase, config] of Object.entries(lifecyclePhases)) {
    if (config.statuses.includes(status)) {
      return phase as LifecyclePhase;
    }
  }
  return "review"; // default fallback
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Extract PR number from a GitHub PR URL. */
export function prNumber(url: string): string | null {
  const match = url.match(/\/pull\/(\d+)/);
  return match?.[1] ?? null;
}

/** Extract owner/repo from a GitHub PR URL. */
export function prRepo(url: string): string | null {
  const match = url.match(/github\.com\/([^/]+\/[^/]+)\/pull/);
  return match?.[1] ?? null;
}

/** Extract just the repo name (no owner) from a GitHub PR URL. */
export function prRepoShort(url: string): string | null {
  const full = prRepo(url);
  return full?.split("/")[1] ?? null;
}

/** Get the task from task list. */
export function getTask(taskId: string, tasks: Task[]): Task | undefined {
  if (!taskId) return undefined;
  return tasks.find((t) => t.id === taskId);
}

// ---------------------------------------------------------------------------
// Request Changes action (needs state for dialog)
// ---------------------------------------------------------------------------

function RequestChangesAction({
  entryId,
  onDone,
}: {
  entryId: string;
  onDone?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [feedback, setFeedback] = useState("");
  const [submitting, setSubmitting] = useState(false);

  async function handleSubmit() {
    if (!feedback.trim()) return;
    setSubmitting(true);
    try {
      await requestChanges(entryId, feedback.trim(), feedback.trim());
      setOpen(false);
      setFeedback("");
      onDone?.();
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <>
      <Button
        variant="ghost"
        size="sm"
        className="h-7 gap-1 border-r border-border rounded-none"
        onClick={() => setOpen(true)}
      >
        <MessageSquare className="h-3.5 w-3.5" />
        Changes
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Request Changes</DialogTitle>
            <DialogDescription>
              Describe what changes are needed. This feedback will be sent back to the agent.
            </DialogDescription>
          </DialogHeader>
          <Textarea
            placeholder="Describe the changes needed..."
            value={feedback}
            onChange={(e) => setFeedback(e.target.value)}
            rows={4}
          />
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="outline" size="sm">Cancel</Button>
            </DialogClose>
            <Button
              size="sm"
              onClick={handleSubmit}
              disabled={!feedback.trim() || submitting}
            >
              {submitting ? "Submitting..." : "Request Changes"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

// ---------------------------------------------------------------------------
// Column definitions
// Row layout: [PR #] [PR title] [project] [status] [linked issue] [queued] [actions]
// ---------------------------------------------------------------------------

export const columns: ColumnDef<MergeQueueEntry>[] = [
  // PR number (extracted from pr_url)
  {
    accessorKey: "pr_url",
    header: "PR",
    cell: ({ row }) => {
      const url = row.original.pr_url;
      const num = prNumber(url);
      if (!num) {
        return <span className="text-muted-foreground">&mdash;</span>;
      }
      return (
        <a
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex items-center gap-1 text-blue-400 hover:underline text-sm font-mono"
        >
          #{num}
          <ExternalLink className="h-3 w-3" />
        </a>
      );
    },
  },

  // PR title (from linked task, or fallback to PR link)
  {
    id: "title",
    header: "Title",
    cell: ({ row, table }) => {
      const task = getTask(row.original.task_id, (table.options.meta as { tasks?: Task[] })?.tasks ?? []);
      if (task) {
        return (
          <Link
            to={`/tasks/${row.original.task_id}`}
            className="text-sm hover:underline truncate max-w-[400px] block"
          >
            {task.title}
          </Link>
        );
      }
      // No linked task — show PR repo/number as fallback
      const repo = prRepo(row.original.pr_url);
      const num = prNumber(row.original.pr_url);
      const label = repo && num ? `${repo}#${num}` : row.original.pr_url;
      return (
        <a
          href={row.original.pr_url}
          target="_blank"
          rel="noopener noreferrer"
          className="text-sm text-muted-foreground hover:underline truncate max-w-[400px] block"
        >
          {label}
        </a>
      );
    },
  },

  // Project name
  {
    id: "project",
    header: "Project",
    cell: ({ row, table }) => {
      const meta = table.options.meta as { tasks?: Task[]; projects?: Project[] } | undefined;
      const task = getTask(row.original.task_id, meta?.tasks ?? []);
      const projects = meta?.projects ?? [];
      let name = task?.project ? projectLabel(task.project, projects) : null;
      // Fall back to extracting repo from PR URL
      if (!name) {
        name = prRepoShort(row.original.pr_url) ?? "—";
      }
      const short = name.includes("/") ? name.split("/")[1] : name;
      return <span className="text-sm text-muted-foreground">{short}</span>;
    },
  },

  // Status
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => statusBadge(row.original.status),
    sortingFn: (rowA, rowB) => {
      const a = statusSortOrder[rowA.original.status] ?? 99;
      const b = statusSortOrder[rowB.original.status] ?? 99;
      return a - b;
    },
  },

  // Queue position (for approved/merging entries)
  {
    accessorKey: "queue_position",
    header: "Position",
    cell: ({ row }) => {
      const pos = row.original.queue_position;
      if (pos === undefined || pos === null) {
        return <span className="text-muted-foreground">&mdash;</span>;
      }
      return (
        <span className="text-xs font-mono text-muted-foreground">
          #{pos}
        </span>
      );
    },
    sortingFn: (rowA, rowB) => {
      const a = rowA.original.queue_position ?? Infinity;
      const b = rowB.original.queue_position ?? Infinity;
      return a - b;
    },
  },

  // Linked issue (from task source)
  {
    id: "issue",
    header: "Issue",
    cell: ({ row, table }) => {
      const task = getTask(row.original.task_id, (table.options.meta as { tasks?: Task[] })?.tasks ?? []);
      if (!task) return <span className="text-muted-foreground">&mdash;</span>;
      const { source } = task;
      if (source.type === "github_issue") {
        const url = `https://github.com/${source.owner}/${source.repo}/issues/${source.number}`;
        return (
          <a
            href={url}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground"
          >
            #{source.number}
            <ExternalLink className="h-3 w-3" />
          </a>
        );
      }
      return <span className="text-muted-foreground">&mdash;</span>;
    },
  },

  // Queued time
  {
    accessorKey: "queued_at",
    header: "Queued",
    cell: ({ row }) => (
      <span className="text-xs text-muted-foreground whitespace-nowrap">
        {formatRelativeTime(row.original.queued_at)}
      </span>
    ),
  },

  // Actions
  {
    id: "actions",
    header: "Actions",
    cell: ({ row, table }) => {
      const entry = row.original;
      if (entry.status !== "pending") return null;

      const refresh = (table.options.meta as { refreshSnapshot: () => Promise<void> })
        ?.refreshSnapshot;

      async function handleApprove() {
        await approveMerge(entry.id);
        refresh?.();
      }

      async function handleReject() {
        await rejectMerge(entry.id);
        refresh?.();
      }

      return (
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
          <RequestChangesAction entryId={entry.id} onDone={refresh} />
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
      );
    },
  },
];
