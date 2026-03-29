import { useMemo, useState, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import {
  ArrowDown,
  ArrowRight,
  ArrowUp,
  ChevronRight,
  ExternalLink,
  MessageSquareWarning,
  Minus,
  Plus,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { cn, formatRelativeTime } from "@/lib/utils";
import { createIssue } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  ListView,
  ListEmptyState,
} from "@/components/ui/list-view";
import {
  ListHeader,
  ListHeaderTabs,
} from "@/components/ui/list-header";
import {
  ListRow,
  IconCell,
  IdCell,
  TextCell,
  BadgeCell,
  TimeCell,
  LinkCell,
  ProjectCell,
  ListRowGroup,
} from "@/components/ui/list-row";
import type { MergeQueueEntry, Task, TaskState } from "@/lib/types";
import { taskStateMeta, stateSortOrder } from "./columns";
import { SortableTaskList } from "./sortable-task-list";

// ---------------------------------------------------------------------------
// Tab definitions
// ---------------------------------------------------------------------------

type TabKey = "all" | "active" | "backlog" | "completed" | "queue";

const ACTIVE_STATES: TaskState[] = [
  "running",
  "question",
  "testing",
  "awaiting_merge",
  "conflict",
];

const BACKLOG_STATES: TaskState[] = ["waiting", "blocked"];

const COMPLETED_STATES: TaskState[] = ["completed", "failed", "cancelled"];

// States eligible for dispatch queue (can be reordered)
const QUEUE_STATES: TaskState[] = [
  "waiting",
  "changes_requested",
];

function filterByTab(tasks: Task[], tab: TabKey): Task[] {
  switch (tab) {
    case "active":
      return tasks.filter((t) => ACTIVE_STATES.includes(t.state));
    case "backlog":
      return tasks.filter((t) => BACKLOG_STATES.includes(t.state));
    case "completed":
      return tasks.filter((t) => COMPLETED_STATES.includes(t.state));
    case "queue":
      return tasks.filter((t) => QUEUE_STATES.includes(t.state));
    default:
      return tasks;
  }
}

// Sort tasks by dispatch priority (matching backend dispatcher logic)
function sortByDispatchPriority(tasks: Task[]): Task[] {
  return [...tasks].sort((a, b) => {
    // 1. ChangesRequested tasks supersede Waiting tasks
    const crA = a.state === "changes_requested";
    const crB = b.state === "changes_requested";
    if (crA !== crB) return crB ? 1 : -1;

    // 2. Explicit priority: lower number first, null sorts last
    const priA = a.priority ?? Number.MAX_SAFE_INTEGER;
    const priB = b.priority ?? Number.MAX_SAFE_INTEGER;
    if (priA !== priB) return priA - priB;

    // 3. Recency: newer source_created_at first (fall back to created_at)
    const timeA = new Date(a.created_at).getTime();
    const timeB = new Date(b.created_at).getTime();
    return timeB - timeA;
  });
}

// ---------------------------------------------------------------------------
// Group tasks by state
// ---------------------------------------------------------------------------

interface TaskGroup {
  state: TaskState;
  tasks: Task[];
}

function groupByState(tasks: Task[]): TaskGroup[] {
  const groups = new Map<TaskState, Task[]>();
  for (const task of tasks) {
    const existing = groups.get(task.state);
    if (existing) {
      existing.push(task);
    } else {
      groups.set(task.state, [task]);
    }
  }

  return Array.from(groups.entries())
    .sort(([a], [b]) => (stateSortOrder[a] ?? 999) - (stateSortOrder[b] ?? 999))
    .map(([state, tasks]) => ({
      state,
      tasks: tasks.sort(
        (a, b) => new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime()
      ),
    }));
}

// ---------------------------------------------------------------------------
// PR number helper
// ---------------------------------------------------------------------------

/** Extract PR number from a GitHub PR URL. */
function prNumber(url: string): string | null {
  const match = url.match(/\/pull\/(\d+)/);
  return match?.[1] ?? null;
}

// ---------------------------------------------------------------------------
// Priority indicator
// ---------------------------------------------------------------------------

const priorityConfig: Record<
  number,
  { icon: typeof ArrowUp; className: string }
> = {
  1: { icon: ArrowUp, className: "text-red-500" },
  2: { icon: ArrowRight, className: "text-yellow-500" },
  3: { icon: ArrowDown, className: "text-blue-500" },
};

function PriorityIcon({ priority }: { priority: number | null }) {
  if (priority == null) {
    return <Minus className="h-3.5 w-3.5 text-muted-foreground/50" />;
  }
  const config = priorityConfig[priority];
  if (!config) {
    return <Minus className="h-3.5 w-3.5 text-muted-foreground/50" />;
  }
  const Icon = config.icon;
  return <Icon className={cn("h-3.5 w-3.5", config.className)} />;
}

// ---------------------------------------------------------------------------
// Task row
// ---------------------------------------------------------------------------

function TaskRow({
  task,
  projectName,
  prUrl,
}: {
  task: Task;
  projectName: string;
  prUrl?: string;
}) {
  const navigate = useNavigate();
  const meta = taskStateMeta[task.state];
  const StateIcon = meta?.icon;

  const idLabel =
    task.source.type === "github_issue" || task.source.type === "github_pr"
      ? `#${task.source.number}`
      : task.id.slice(0, 8);

  return (
    <ListRow as="button" onRowClick={() => navigate(`/tasks/${task.id}`)}>
      <IconCell>
        <PriorityIcon priority={task.priority} />
      </IconCell>
      <IdCell>{idLabel}</IdCell>
      {StateIcon && (
        <IconCell>
          <StateIcon className={cn("h-4 w-4", meta.color)} />
        </IconCell>
      )}
      <TextCell>{task.title}</TextCell>
      {task.rejection_feedback && (
        <IconCell>
          <span title="Has rejection feedback">
            <MessageSquareWarning className="h-3.5 w-3.5 text-amber-500" />
          </span>
        </IconCell>
      )}
      {task.labels.map((label) => (
        <BadgeCell key={label}>
          <Badge variant="outline" className="text-xs">
            {label}
          </Badge>
        </BadgeCell>
      ))}
      {prUrl && prNumber(prUrl) && (
        <LinkCell href={prUrl} icon={<ExternalLink className="h-3 w-3" />}>
          #{prNumber(prUrl)}
        </LinkCell>
      )}
      <ProjectCell>{projectName}</ProjectCell>
      <TimeCell>{formatRelativeTime(task.updated_at)}</TimeCell>
    </ListRow>
  );
}

// ---------------------------------------------------------------------------
// Collapsible group
// ---------------------------------------------------------------------------

function TaskGroupSection({
  group,
  projectIdToRepo,
  taskToPrUrl,
  defaultOpen,
}: {
  group: TaskGroup;
  projectIdToRepo: Record<string, string>;
  taskToPrUrl: Record<string, string>;
  defaultOpen: boolean;
}) {
  const [isOpen, setIsOpen] = useState(defaultOpen);
  const meta = taskStateMeta[group.state];
  const StateIcon = meta?.icon;

  const header = (
    <>
      <ChevronRight
        className={cn(
          "h-3.5 w-3.5 text-muted-foreground transition-transform",
          isOpen && "rotate-90"
        )}
      />
      {StateIcon && <StateIcon className={cn("h-4 w-4", meta.color)} />}
      <span className="font-medium">{meta?.label ?? group.state}</span>
      <span className="text-xs text-muted-foreground">{group.tasks.length}</span>
    </>
  );

  return (
    <ListRowGroup
      header={header}
      isOpen={isOpen}
      onToggle={() => setIsOpen(!isOpen)}
    >
      {group.tasks.map((task) => (
        <TaskRow
          key={task.id}
          task={task}
          projectName={projectIdToRepo[task.project] ?? task.project}
          prUrl={taskToPrUrl[task.id]}
        />
      ))}
    </ListRowGroup>
  );
}

// ---------------------------------------------------------------------------
// Tasks Page
// ---------------------------------------------------------------------------

export function TasksPage() {
  const { filteredTasks, selectedProject, snapshot, refreshSnapshot } = useAppState();
  const [activeTab, setActiveTab] = useState<TabKey>("active");
  const [search, setSearch] = useState("");

  // Callback when tasks are reordered in queue view
  const handleReorder = useCallback(() => {
    // Refresh to get updated priorities from server
    refreshSnapshot();
  }, [refreshSnapshot]);

  // New task dialog state
  const [newTaskOpen, setNewTaskOpen] = useState(false);
  const [newTaskProjectId, setNewTaskProjectId] = useState<string>("");
  const [newTaskTitle, setNewTaskTitle] = useState("");
  const [newTaskBody, setNewTaskBody] = useState("");
  const [newTaskLabels, setNewTaskLabels] = useState("");
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);

  const projects = snapshot?.projects ?? [];
  const mergeQueue = snapshot?.merge_queue ?? [];

  const projectIdToRepo = useMemo(() => {
    const map: Record<string, string> = {};
    for (const p of projects) {
      map[p.id] = p.repo;
    }
    return map;
  }, [projects]);

  // Map task IDs to PR URLs from merge queue entries
  const taskToPrUrl = useMemo(() => {
    const map: Record<string, string> = {};
    for (const entry of mergeQueue) {
      if (entry.pr_url) {
        map[entry.task_id] = entry.pr_url;
      }
    }
    return map;
  }, [mergeQueue]);

  // Filter by tab and search
  const displayedTasks = useMemo(() => {
    let tasks = filterByTab(filteredTasks, activeTab);
    if (search.trim()) {
      const q = search.toLowerCase();
      tasks = tasks.filter(
        (t) =>
          t.title.toLowerCase().includes(q) ||
          t.id.toLowerCase().includes(q)
      );
    }
    return tasks;
  }, [filteredTasks, activeTab, search]);

  const groups = useMemo(() => groupByState(displayedTasks), [displayedTasks]);

  // Tab counts
  const counts = useMemo(() => {
    const all = filteredTasks.length;
    const active = filteredTasks.filter((t) => ACTIVE_STATES.includes(t.state)).length;
    const backlog = filteredTasks.filter((t) => BACKLOG_STATES.includes(t.state)).length;
    const completed = filteredTasks.filter((t) => COMPLETED_STATES.includes(t.state)).length;
    const queue = filteredTasks.filter((t) => QUEUE_STATES.includes(t.state)).length;
    return { all, active, backlog, completed, queue };
  }, [filteredTasks]);

  // Tasks sorted for queue view (dispatch order)
  const queueTasks = useMemo(() => {
    let tasks = filterByTab(filteredTasks, "queue");
    if (search.trim()) {
      const q = search.toLowerCase();
      tasks = tasks.filter(
        (t) =>
          t.title.toLowerCase().includes(q) ||
          t.id.toLowerCase().includes(q)
      );
    }
    return sortByDispatchPriority(tasks);
  }, [filteredTasks, search]);

  // Reset dialog state when opening
  const handleOpenNewTask = () => {
    setNewTaskProjectId(selectedProject ?? (projects[0]?.id ?? ""));
    setNewTaskTitle("");
    setNewTaskBody("");
    setNewTaskLabels("");
    setCreateError(null);
    setNewTaskOpen(true);
  };

  // Handle create task
  const handleCreateTask = async () => {
    if (!newTaskTitle.trim()) {
      setCreateError("Title is required");
      return;
    }
    if (!newTaskProjectId) {
      setCreateError("Please select a project");
      return;
    }

    setCreating(true);
    setCreateError(null);

    try {
      const labels = newTaskLabels
        .split(",")
        .map((l) => l.trim())
        .filter((l) => l.length > 0);

      await createIssue({
        project_id: newTaskProjectId,
        title: newTaskTitle.trim(),
        body: newTaskBody.trim() || undefined,
        labels: labels.length > 0 ? labels : undefined,
      });

      setNewTaskOpen(false);
      await refreshSnapshot();
    } catch (e) {
      setCreateError(e instanceof Error ? e.message : "Failed to create task");
    } finally {
      setCreating(false);
    }
  };

  const tabsConfig = [
    { key: "queue" as const, label: "Queue", count: counts.queue },
    { key: "active" as const, label: "Active", count: counts.active },
    { key: "backlog" as const, label: "Backlog", count: counts.backlog },
    { key: "completed" as const, label: "Completed", count: counts.completed },
    { key: "all" as const, label: "All", count: counts.all },
  ];

  const headerTabs = (
    <ListHeaderTabs
      tabs={tabsConfig}
      activeTab={activeTab}
      onTabChange={setActiveTab}
    />
  );

  const headerActions = (
    <Button
      size="sm"
      className="h-7 text-xs gap-1"
      onClick={handleOpenNewTask}
      disabled={projects.length === 0}
    >
      <Plus className="h-3.5 w-3.5" />
      New Task
    </Button>
  );

  const renderContent = () => {
    if (activeTab === "queue") {
      if (queueTasks.length === 0) {
        return (
          <ListEmptyState
            message={search ? "No tasks match your search." : "No tasks in queue."}
          />
        );
      }
      return (
        <div className="py-2">
          <div className="px-4 py-2 text-xs text-muted-foreground border-b border-border">
            Drag tasks to reorder the dispatch queue. Tasks at the top will be picked up first.
          </div>
          <SortableTaskList
            tasks={queueTasks}
            projectIdToRepo={projectIdToRepo}
            onReorder={handleReorder}
          />
        </div>
      );
    }

    if (groups.length === 0) {
      return (
        <ListEmptyState
          message={search ? "No tasks match your search." : "No tasks."}
        />
      );
    }

    return (
      <div>
        {groups.map((group) => (
          <TaskGroupSection
            key={group.state}
            group={group}
            projectIdToRepo={projectIdToRepo}
            taskToPrUrl={taskToPrUrl}
            defaultOpen={!COMPLETED_STATES.includes(group.state)}
          />
        ))}
      </div>
    );
  };

  return (
    <>
    <ListView
      header={
        <ListHeader
          title={selectedProject ? projectIdToRepo[selectedProject] ?? "Project" : "Tasks"}
          tabs={headerTabs}
          search={{
            value: search,
            onChange: setSearch,
          }}
          actions={headerActions}
        />
      }
    >
      {renderContent()}
    </ListView>

    {/* New Task Dialog */}
    <Dialog open={newTaskOpen} onOpenChange={(open) => {
        if (!creating) setNewTaskOpen(open);
      }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create new task</DialogTitle>
          <DialogDescription>
            Create a GitHub issue that will become a task. The poller will pick it up on the next cycle.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            handleCreateTask();
          }}
        >
          <div className="space-y-4 py-3">
            {/* Project selector */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Project</label>
              <Select
                value={newTaskProjectId}
                onValueChange={setNewTaskProjectId}
              >
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Select a project" />
                </SelectTrigger>
                <SelectContent>
                  {projects.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.repo}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* Title */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Title</label>
              <Input
                placeholder="Issue title..."
                value={newTaskTitle}
                onChange={(e) => {
                  setNewTaskTitle(e.target.value);
                  setCreateError(null);
                }}
                disabled={creating}
                autoFocus
              />
            </div>

            {/* Body */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Description (optional)</label>
              <Textarea
                placeholder="Issue description in markdown..."
                value={newTaskBody}
                onChange={(e) => setNewTaskBody(e.target.value)}
                disabled={creating}
                className="min-h-24"
              />
            </div>

            {/* Labels */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Labels (optional)</label>
              <Input
                placeholder="bug, enhancement, help wanted"
                value={newTaskLabels}
                onChange={(e) => setNewTaskLabels(e.target.value)}
                disabled={creating}
              />
              <p className="text-xs text-muted-foreground">
                Comma-separated list of labels
              </p>
            </div>

            {createError && (
              <p className="text-sm text-red-400">{createError}</p>
            )}
          </div>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setNewTaskOpen(false)}
              disabled={creating}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={creating || !newTaskTitle.trim() || !newTaskProjectId}
            >
              {creating ? "Creating..." : "Create Task"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
    </>
  );
}
