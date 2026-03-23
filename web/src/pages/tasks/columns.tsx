import type { ComponentType } from "react";
import {
  Ban,
  CheckCircle2,
  Circle,
  FlaskConical,
  GitMerge,
  HelpCircle,
  MinusCircle,
  XCircle,
  AlertTriangle,
} from "lucide-react";
import { Spinner } from "@/components/ui/spinner";
import type { TaskState } from "@/lib/types";

// ---------------------------------------------------------------------------
// Task state metadata
// ---------------------------------------------------------------------------

export interface TaskStateMeta {
  label: string;
  icon: ComponentType<{ className?: string }>;
  /** Full className for badge rendering (color + border + bg) */
  className: string;
  /** Just the text color class, for inline icon usage */
  color: string;
}

// State priority for sorting (lower number = higher priority / shown first)
export const stateSortOrder: Record<TaskState, number> = {
  running: 0,
  question: 1,
  conflict: 2,
  testing: 3,
  waiting: 4,
  blocked: 5,
  awaiting_merge: 6,
  completed: 7,
  failed: 8,
  cancelled: 9,
};

export const taskStateMeta: Record<TaskState, TaskStateMeta> = {
  waiting: {
    label: "Waiting",
    icon: Circle,
    className: "text-muted-foreground border-muted",
    color: "text-muted-foreground",
  },
  blocked: {
    label: "Blocked",
    icon: Ban,
    className: "text-muted-foreground border-muted",
    color: "text-muted-foreground",
  },
  running: {
    label: "Running",
    icon: Spinner,
    className: "text-blue-500 border-blue-500/30 bg-blue-500/10",
    color: "text-blue-500",
  },
  question: {
    label: "Question",
    icon: HelpCircle,
    className: "text-yellow-500 border-yellow-500/30 bg-yellow-500/10",
    color: "text-yellow-500",
  },
  testing: {
    label: "Testing",
    icon: FlaskConical,
    className: "text-purple-500 border-purple-500/30 bg-purple-500/10",
    color: "text-purple-500",
  },
  awaiting_merge: {
    label: "Changes Submitted",
    icon: GitMerge,
    className: "text-orange-500 border-orange-500/30 bg-orange-500/10",
    color: "text-orange-500",
  },
  conflict: {
    label: "Conflict",
    icon: AlertTriangle,
    className: "text-red-500 border-red-500/30 bg-red-500/10",
    color: "text-red-500",
  },
  completed: {
    label: "Completed",
    icon: CheckCircle2,
    className: "text-green-500 border-green-500/30 bg-green-500/10",
    color: "text-green-500",
  },
  failed: {
    label: "Failed",
    icon: XCircle,
    className: "text-red-500 border-red-500/30 bg-red-500/10",
    color: "text-red-500",
  },
  cancelled: {
    label: "Cancelled",
    icon: MinusCircle,
    className: "text-gray-500 border-gray-500/30 bg-gray-500/10",
    color: "text-gray-500",
  },
};
