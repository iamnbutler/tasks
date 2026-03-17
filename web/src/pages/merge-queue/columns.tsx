import { type ColumnDef } from "@tanstack/react-table";
import { Link } from "react-router-dom";
import { ExternalLink, Check, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { formatRelativeTime } from "@/lib/utils";
import { approveMerge, rejectMerge } from "@/lib/api";
import type { MergeQueueEntry, MergeStatus } from "@/lib/types";

// ---------------------------------------------------------------------------
// Status badge
// ---------------------------------------------------------------------------

function statusBadge(status: MergeStatus) {
  switch (status) {
    case "pending":
      return <Badge className="bg-yellow-600 text-white">{status}</Badge>;
    case "approved":
      return <Badge className="bg-green-600 text-white">{status}</Badge>;
    case "rejected":
      return <Badge className="bg-red-600 text-white">{status}</Badge>;
    case "merged":
      return <Badge className="bg-blue-600 text-white">{status}</Badge>;
    case "conflict":
      return <Badge className="bg-orange-600 text-white">{status}</Badge>;
    default:
      return <Badge variant="outline">{status}</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Column definitions
// ---------------------------------------------------------------------------

export const columns: ColumnDef<MergeQueueEntry>[] = [
  {
    accessorKey: "id",
    header: "ID",
    cell: ({ row }) => (
      <span className="font-mono text-xs">{row.original.id.slice(0, 8)}</span>
    ),
  },
  {
    accessorKey: "task_id",
    header: "Task",
    cell: ({ row }) => (
      <Link
        to={`/tasks/${row.original.task_id}`}
        className="font-mono text-xs text-blue-400 hover:underline"
      >
        {row.original.task_id.slice(0, 8)}...
      </Link>
    ),
  },
  {
    accessorKey: "pr_url",
    header: "PR",
    cell: ({ row }) => {
      const url = row.original.pr_url;
      if (!url) {
        return <span className="text-muted-foreground">&mdash;</span>;
      }
      return (
        <a
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex items-center gap-1 text-blue-400 hover:underline text-xs"
        >
          PR
          <ExternalLink className="h-3 w-3" />
        </a>
      );
    },
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => statusBadge(row.original.status),
  },
  {
    accessorKey: "queued_at",
    header: "Queued",
    cell: ({ row }) => (
      <span className="text-xs text-muted-foreground whitespace-nowrap">
        {formatRelativeTime(row.original.queued_at)}
      </span>
    ),
  },
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
