import type { ColumnDef } from "@tanstack/react-table";
import {
  ArrowDown,
  ArrowRight,
  ArrowUp,
  Ban,
  CheckCircle2,
  Circle,
  FlaskConical,
  GitMerge,
  HelpCircle,
  Loader,
  Minus,
  MinusCircle,
  MoreHorizontal,
  XCircle,
  AlertTriangle,
} from "lucide-react";
import type { ComponentType } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn, formatRelativeTime } from "@/lib/utils";
import type { Task, TaskState } from "@/lib/types";
import { DataTableColumnHeader } from "./data-table-column-header";

// ---------------------------------------------------------------------------
// Task state metadata
// ---------------------------------------------------------------------------

export interface TaskStateMeta {
  label: string;
  icon: ComponentType<{ className?: string }>;
  className: string;
}

// State priority for sorting (lower number = higher priority, appears first)
// Active/actionable states at top, terminal states at bottom
export const taskStatePriority: Record<TaskState, number> = {
  awaiting_merge: 0,
  running: 1,
  question: 2,
  conflict: 3,
  testing: 4,
  waiting: 5,
  blocked: 6,
  completed: 7,
  failed: 8,
  cancelled: 9,
};

export const taskStateMeta: Record<TaskState, TaskStateMeta> = {
  waiting: {
    label: "Waiting",
    icon: Circle,
    className: "text-muted-foreground border-muted",
  },
  blocked: {
    label: "Blocked",
    icon: Ban,
    className: "text-muted-foreground border-muted",
  },
  running: {
    label: "Running",
    icon: ({ className, ...props }: { className?: string }) => (
      <Loader className={cn("animate-spin", className)} {...props} />
    ),
    className: "text-blue-500 border-blue-500/30 bg-blue-500/10",
  },
  question: {
    label: "Question",
    icon: HelpCircle,
    className: "text-yellow-500 border-yellow-500/30 bg-yellow-500/10",
  },
  testing: {
    label: "Testing",
    icon: FlaskConical,
    className: "text-purple-500 border-purple-500/30 bg-purple-500/10",
  },
  awaiting_merge: {
    label: "Awaiting Merge",
    icon: GitMerge,
    className: "text-orange-500 border-orange-500/30 bg-orange-500/10",
  },
  conflict: {
    label: "Conflict",
    icon: AlertTriangle,
    className: "text-red-500 border-red-500/30 bg-red-500/10",
  },
  completed: {
    label: "Completed",
    icon: CheckCircle2,
    className: "text-green-500 border-green-500/30 bg-green-500/10",
  },
  failed: {
    label: "Failed",
    icon: XCircle,
    className: "text-red-500 border-red-500/30 bg-red-500/10",
  },
  cancelled: {
    label: "Cancelled",
    icon: MinusCircle,
    className: "text-gray-500 border-gray-500/30 bg-gray-500/10",
  },
};

// ---------------------------------------------------------------------------
// Priority helpers
// ---------------------------------------------------------------------------

const priorityConfig: Record<
  number,
  { icon: ComponentType<{ className?: string }>; className: string; label: string }
> = {
  1: { icon: ArrowUp, className: "text-red-500", label: "High" },
  2: { icon: ArrowRight, className: "text-yellow-500", label: "Medium" },
  3: { icon: ArrowDown, className: "text-blue-500", label: "Low" },
};

// ---------------------------------------------------------------------------
// Column definitions
// ---------------------------------------------------------------------------

export const columns: ColumnDef<Task>[] = [
  // Select
  {
    id: "select",
    header: ({ table }) => (
      <Checkbox
        checked={
          table.getIsAllPageRowsSelected() ||
          (table.getIsSomePageRowsSelected() && "indeterminate")
        }
        onCheckedChange={(value) => table.toggleAllPageRowsSelected(!!value)}
        aria-label="Select all"
        className="translate-y-[2px]"
      />
    ),
    cell: ({ row }) => (
      <Checkbox
        checked={row.getIsSelected()}
        onCheckedChange={(value) => row.toggleSelected(!!value)}
        aria-label="Select row"
        className="translate-y-[2px]"
      />
    ),
    enableSorting: false,
    enableHiding: false,
  },

  // ID
  {
    accessorKey: "id",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="ID" />
    ),
    cell: ({ row }) => {
      const source = row.original.source;
      const label =
        source.type === "github_issue" || source.type === "github_pr"
          ? `#${source.number}`
          : row.original.id.slice(0, 8);
      return (
        <span className="font-mono text-xs text-muted-foreground">
          {label}
        </span>
      );
    },
    enableSorting: false,
    enableHiding: false,
  },

  // Title
  {
    accessorKey: "title",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="Title" />
    ),
    cell: ({ row }) => {
      const labels = row.original.labels;
      return (
        <div className="flex items-center gap-2">
          <a
            href={`/tasks/${row.original.id}`}
            className="max-w-[500px] truncate font-medium hover:underline"
          >
            {row.getValue<string>("title")}
          </a>
          {labels.map((label) => (
            <Badge key={label} variant="outline" className="text-xs">
              {label}
            </Badge>
          ))}
        </div>
      );
    },
  },

  // State
  {
    accessorKey: "state",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="State" />
    ),
    cell: ({ row }) => {
      const state = row.getValue<TaskState>("state");
      const meta = taskStateMeta[state];
      if (!meta) return null;
      const Icon = meta.icon;
      return (
        <Badge variant="outline" className={cn("gap-1", meta.className)}>
          <Icon className="h-3.5 w-3.5" />
          {meta.label}
        </Badge>
      );
    },
    filterFn: (row, id, value: string[]) => {
      return value.includes(row.getValue(id));
    },
    sortingFn: (rowA, rowB) => {
      const stateA = rowA.getValue<TaskState>("state");
      const stateB = rowB.getValue<TaskState>("state");
      return taskStatePriority[stateA] - taskStatePriority[stateB];
    },
  },

  // Project
  {
    accessorKey: "project",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="Project" />
    ),
    cell: ({ row }) => (
      <span className="text-sm">{row.getValue<string>("project")}</span>
    ),
    filterFn: (row, id, value: string[]) => {
      return value.includes(row.getValue(id));
    },
  },

  // Priority
  {
    accessorKey: "priority",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="Priority" />
    ),
    cell: ({ row }) => {
      const priority = row.getValue<number | null>("priority");
      if (priority == null) {
        return (
          <span className="flex items-center gap-1 text-gray-500">
            <Minus className="h-4 w-4" />
            <span className="text-xs">None</span>
          </span>
        );
      }
      const config = priorityConfig[priority];
      if (!config) {
        return <span className="text-xs text-muted-foreground">{priority}</span>;
      }
      const Icon = config.icon;
      return (
        <span className={cn("flex items-center gap-1", config.className)}>
          <Icon className="h-4 w-4" />
          <span className="text-xs">{config.label}</span>
        </span>
      );
    },
    sortingFn: (rowA, rowB) => {
      const a = rowA.getValue<number | null>("priority") ?? 999;
      const b = rowB.getValue<number | null>("priority") ?? 999;
      return a - b;
    },
  },

  // Updated
  {
    accessorKey: "updated_at",
    header: ({ column }) => (
      <DataTableColumnHeader column={column} title="Updated" />
    ),
    cell: ({ row }) => (
      <span className="text-sm text-muted-foreground">
        {formatRelativeTime(row.getValue<string>("updated_at"))}
      </span>
    ),
  },

  // Actions
  {
    id: "actions",
    cell: ({ row }) => {
      const task = row.original;
      return (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              className="flex h-8 w-8 p-0 data-[state=open]:bg-muted"
            >
              <MoreHorizontal className="h-4 w-4" />
              <span className="sr-only">Open menu</span>
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-[160px]">
            <DropdownMenuItem
              onClick={() => navigator.clipboard.writeText(task.id)}
            >
              Copy task ID
            </DropdownMenuItem>
            <DropdownMenuItem asChild>
              <a href={`/tasks/${task.id}`}>View details</a>
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      );
    },
  },
];
