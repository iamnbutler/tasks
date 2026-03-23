import { useCallback, useEffect, useMemo, useState } from "react";
import { Cell, Pie, PieChart, Bar, BarChart, XAxis, YAxis } from "recharts";
import { AlertTriangle, RefreshCw, Sparkles, Bell, ExternalLink } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { complete } from "@/lib/api";
import type { Task, TaskState, MergeQueueEntry, MergeStatus, Event, Project } from "@/lib/types";
import { formatRelativeTime, projectLabel } from "@/lib/utils";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Spinner } from "@/components/ui/spinner";

// ---------------------------------------------------------------------------
// Task state chart config
// ---------------------------------------------------------------------------

const taskStateColors: Record<TaskState, string> = {
  waiting: "hsl(var(--muted-foreground))",
  blocked: "hsl(var(--muted-foreground))",
  running: "hsl(217, 91%, 60%)",
  question: "hsl(45, 93%, 47%)",
  testing: "hsl(270, 50%, 60%)",
  awaiting_merge: "hsl(25, 95%, 53%)",
  conflict: "hsl(25, 95%, 53%)",
  completed: "hsl(142, 71%, 45%)",
  failed: "hsl(0, 84%, 60%)",
  cancelled: "hsl(0, 0%, 50%)",
};

const taskStateLabels: Record<TaskState, string> = {
  waiting: "Waiting",
  blocked: "Blocked",
  running: "Running",
  question: "Question",
  testing: "Testing",
  awaiting_merge: "Awaiting Merge",
  conflict: "Conflict",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
};

const taskChartConfig: ChartConfig = Object.fromEntries(
  Object.entries(taskStateLabels).map(([key, label]) => [
    key,
    { label, color: taskStateColors[key as TaskState] },
  ])
);

// ---------------------------------------------------------------------------
// Merge status chart config
// ---------------------------------------------------------------------------

const mergeStatusColors: Record<MergeStatus, string> = {
  pending: "hsl(45, 93%, 47%)",
  approved: "hsl(142, 71%, 45%)",
  rejected: "hsl(0, 84%, 60%)",
  merged: "hsl(217, 91%, 60%)",
  conflict: "hsl(0, 84%, 60%)",
};

const mergeStatusLabels: Record<MergeStatus, string> = {
  pending: "Pending",
  approved: "Approved",
  rejected: "Rejected",
  merged: "Merged",
  conflict: "Conflict",
};

const mergeChartConfig: ChartConfig = Object.fromEntries(
  Object.entries(mergeStatusLabels).map(([key, label]) => [
    key,
    { label, color: mergeStatusColors[key as MergeStatus] },
  ])
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function countByState(tasks: Task[]): { state: TaskState; count: number }[] {
  const counts = new Map<TaskState, number>();
  for (const task of tasks) {
    counts.set(task.state, (counts.get(task.state) ?? 0) + 1);
  }
  return Array.from(counts.entries())
    .map(([state, count]) => ({ state, count }))
    .sort((a, b) => b.count - a.count);
}

function countByMergeStatus(
  entries: MergeQueueEntry[]
): { status: MergeStatus; count: number }[] {
  const counts = new Map<MergeStatus, number>();
  for (const entry of entries) {
    counts.set(entry.status, (counts.get(entry.status) ?? 0) + 1);
  }
  return Array.from(counts.entries())
    .map(([status, count]) => ({ status, count }))
    .sort((a, b) => b.count - a.count);
}

function buildSummaryContext(
  tasks: Task[],
  mergeQueue: MergeQueueEntry[]
): string {
  const taskCounts = countByState(tasks);
  const mergeCounts = countByMergeStatus(mergeQueue);

  const lines: string[] = [
    `Total tasks: ${tasks.length}`,
    "",
    "Task breakdown:",
    ...taskCounts.map((t) => `  ${taskStateLabels[t.state]}: ${t.count}`),
    "",
    `Merge queue size: ${mergeQueue.length}`,
  ];

  if (mergeCounts.length > 0) {
    lines.push("Merge queue breakdown:");
    lines.push(
      ...mergeCounts.map((m) => `  ${mergeStatusLabels[m.status]}: ${m.count}`)
    );
  }

  const recentTasks = tasks
    .sort(
      (a, b) =>
        new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime()
    )
    .slice(0, 5);

  if (recentTasks.length > 0) {
    lines.push("");
    lines.push("Recently active tasks:");
    for (const task of recentTasks) {
      lines.push(
        `  - "${task.title}" (${taskStateLabels[task.state]})`
      );
    }
  }

  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Task State Pie Chart
// ---------------------------------------------------------------------------

function TaskStateChart({ tasks }: { tasks: Task[] }) {
  const data = useMemo(() => countByState(tasks), [tasks]);
  const total = tasks.length;

  if (total === 0) {
    return (
      <div className="flex h-[200px] items-center justify-center text-sm text-muted-foreground">
        No tasks
      </div>
    );
  }

  return (
    <ChartContainer config={taskChartConfig} className="mx-auto aspect-square h-[200px]">
      <PieChart>
        <ChartTooltip
          cursor={false}
          content={<ChartTooltipContent hideLabel />}
        />
        <Pie
          data={data}
          dataKey="count"
          nameKey="state"
          innerRadius={50}
          outerRadius={80}
          strokeWidth={2}
        >
          {data.map((entry) => (
            <Cell
              key={entry.state}
              fill={taskStateColors[entry.state]}
              stroke="hsl(var(--background))"
            />
          ))}
        </Pie>
      </PieChart>
    </ChartContainer>
  );
}

// ---------------------------------------------------------------------------
// Task State Bar Chart (detailed breakdown)
// ---------------------------------------------------------------------------

function TaskStateBarChart({ tasks }: { tasks: Task[] }) {
  const data = useMemo(() => {
    const counts = countByState(tasks);
    return counts.map((c) => ({
      state: taskStateLabels[c.state],
      count: c.count,
      fill: taskStateColors[c.state],
    }));
  }, [tasks]);

  if (data.length === 0) {
    return (
      <div className="flex h-[200px] items-center justify-center text-sm text-muted-foreground">
        No tasks
      </div>
    );
  }

  return (
    <ChartContainer config={taskChartConfig} className="h-[200px] w-full">
      <BarChart data={data} layout="vertical" margin={{ left: 0, right: 16 }}>
        <YAxis
          dataKey="state"
          type="category"
          tickLine={false}
          axisLine={false}
          width={100}
          tick={{ fontSize: 11 }}
        />
        <XAxis type="number" hide />
        <ChartTooltip
          cursor={false}
          content={<ChartTooltipContent hideLabel />}
        />
        <Bar dataKey="count" radius={[0, 4, 4, 0]}>
          {data.map((entry) => (
            <Cell key={entry.state} fill={entry.fill} />
          ))}
        </Bar>
      </BarChart>
    </ChartContainer>
  );
}

// ---------------------------------------------------------------------------
// Merge Queue Chart
// ---------------------------------------------------------------------------

function MergeQueueChart({ entries }: { entries: MergeQueueEntry[] }) {
  const data = useMemo(() => countByMergeStatus(entries), [entries]);
  const total = entries.length;

  if (total === 0) {
    return (
      <div className="flex h-[200px] items-center justify-center text-sm text-muted-foreground">
        Merge queue empty
      </div>
    );
  }

  return (
    <ChartContainer config={mergeChartConfig} className="mx-auto aspect-square h-[200px]">
      <PieChart>
        <ChartTooltip
          cursor={false}
          content={<ChartTooltipContent hideLabel />}
        />
        <Pie
          data={data}
          dataKey="count"
          nameKey="status"
          innerRadius={50}
          outerRadius={80}
          strokeWidth={2}
        >
          {data.map((entry) => (
            <Cell
              key={entry.status}
              fill={mergeStatusColors[entry.status]}
              stroke="hsl(var(--background))"
            />
          ))}
        </Pie>
      </PieChart>
    </ChartContainer>
  );
}

// ---------------------------------------------------------------------------
// AI Summary Component
// ---------------------------------------------------------------------------

function AISummary({
  tasks,
  mergeQueue,
}: {
  tasks: Task[];
  mergeQueue: MergeQueueEntry[];
}) {
  const [summary, setSummary] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const context = useMemo(
    () => buildSummaryContext(tasks, mergeQueue),
    [tasks, mergeQueue]
  );

  const fetchSummary = useCallback(async () => {
    if (tasks.length === 0 && mergeQueue.length === 0) {
      setSummary("No active work to summarize. Add some tasks to get started!");
      return;
    }

    setLoading(true);
    setError(null);
    try {
      const systemPrompt = `You are a project status assistant. Given the current state of tasks and merge queue, provide a brief, conversational summary (2-3 sentences) about what's happening in the project. Focus on:
- What work is actively in progress
- Any bottlenecks or items needing attention
- The overall health of the project

Be concise and helpful. Don't use bullet points - write in natural sentences.`;

      const data = await complete({
        prompt: context,
        system: systemPrompt,
        max_tokens: 256,
      });
      setSummary(data.text);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to generate summary");
    } finally {
      setLoading(false);
    }
  }, [context, tasks.length, mergeQueue.length]);

  useEffect(() => {
    fetchSummary();
  }, [fetchSummary]);

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="space-y-1">
          <CardTitle className="flex items-center gap-2">
            <Sparkles className="h-4 w-4" />
            Project Summary
          </CardTitle>
          <CardDescription>AI-generated overview of current work</CardDescription>
        </div>
        <Button
          variant="ghost"
          size="icon"
          onClick={fetchSummary}
          disabled={loading}
          className="h-8 w-8"
        >
          <RefreshCw className={loading ? "h-4 w-4 animate-spin" : "h-4 w-4"} />
        </Button>
      </CardHeader>
      <CardContent>
        {loading && !summary && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Spinner className="h-4 w-4" />
            Generating summary...
          </div>
        )}
        {error && (
          <div className="flex items-center gap-2 text-sm text-red-500">
            <AlertTriangle className="h-4 w-4" />
            {error}
          </div>
        )}
        {summary && (
          <p className="text-sm leading-relaxed text-muted-foreground">
            {summary}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Escalation event parsing helper
// ---------------------------------------------------------------------------

interface EscalationAlert {
  id: string;
  timestamp: string;
  action: string;
  reasoning?: string;
  prUrl?: string;
  taskId?: string;
  fromMode?: string;
  toMode?: string;
}

function parseEscalationEvents(events: Event[]): EscalationAlert[] {
  return events
    .filter((e) => e.type === "orchestrator:escalation")
    .map((event) => ({
      id: event.id,
      timestamp: event.ts,
      action: typeof event.data?.action === "string" ? event.data.action : "unknown",
      reasoning:
        typeof event.data?.reasoning === "string" ? event.data.reasoning :
        typeof event.data?.reason === "string" ? event.data.reason :
        undefined,
      prUrl: typeof event.data?.pr_url === "string" ? event.data.pr_url : undefined,
      taskId: event.task,
      fromMode: typeof event.data?.from === "string" ? event.data.from : undefined,
      toMode: typeof event.data?.to === "string" ? event.data.to : undefined,
    }))
    .sort((a, b) => b.timestamp.localeCompare(a.timestamp)); // Most recent first
}

function taskLabel(taskId: string | undefined, tasks: Task[], projects: Project[]): string | null {
  if (!taskId) return null;
  const task = tasks.find((t) => t.id === taskId);
  if (!task) return taskId.slice(0, 8);
  const { source } = task;
  const issueNum =
    (source.type === "github_issue" || source.type === "github_pr")
      ? `#${source.number}`
      : taskId.slice(0, 8);
  const proj = projectLabel(task.project, projects);
  const repoName = proj.includes("/") ? proj.split("/")[1] : proj;
  return `${issueNum} (${repoName})`;
}

// ---------------------------------------------------------------------------
// Orchestrator Alerts Component
// ---------------------------------------------------------------------------

function OrchestratorAlerts({
  events,
  tasks,
  projects,
}: {
  events: Event[];
  tasks: Task[];
  projects: Project[];
}) {
  const escalations = useMemo(() => parseEscalationEvents(events), [events]);

  // Show at most 5 recent escalations
  const recentEscalations = escalations.slice(0, 5);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Bell className="h-4 w-4" />
          Orchestrator Notifications
          {escalations.length > 0 && (
            <span className="ml-auto rounded-full bg-yellow-500/20 px-2 py-0.5 text-xs font-medium text-yellow-500">
              {escalations.length}
            </span>
          )}
        </CardTitle>
        <CardDescription>
          System alerts and concerns will appear here
        </CardDescription>
      </CardHeader>
      <CardContent>
        {recentEscalations.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-8 text-center">
            <div className="rounded-full bg-muted p-3">
              <Bell className="h-6 w-6 text-muted-foreground" />
            </div>
            <p className="mt-3 text-sm text-muted-foreground">
              No alerts at this time
            </p>
            <p className="mt-1 text-xs text-muted-foreground/70">
              The orchestrator will notify you of conflicts, failures, or items
              needing attention
            </p>
          </div>
        ) : (
          <div className="space-y-3">
            {recentEscalations.map((alert) => {
              const label = taskLabel(alert.taskId, tasks, projects);
              let title = "Alert";
              if (alert.action === "conflict_needs_human") {
                title = "Conflict Needs Review";
              } else if (alert.action === "mode_lowered") {
                title = "Mode Lowered";
              }

              return (
                <div
                  key={alert.id}
                  className="rounded-md border border-yellow-500/30 bg-yellow-500/10 p-3"
                >
                  <div className="flex items-start gap-2">
                    <AlertTriangle className="h-4 w-4 text-yellow-500 mt-0.5 shrink-0" />
                    <div className="flex-1 min-w-0">
                      <div className="flex items-center gap-2 flex-wrap">
                        <span className="font-medium text-sm text-yellow-400">
                          {title}
                        </span>
                        {label && (
                          <span className="rounded border border-border px-1.5 py-0.5 font-mono text-xs">
                            {label}
                          </span>
                        )}
                        {alert.action === "mode_lowered" && alert.fromMode && alert.toMode && (
                          <span className="rounded border border-yellow-500/30 bg-yellow-500/10 px-1.5 py-0.5 text-xs">
                            {alert.fromMode} → {alert.toMode}
                          </span>
                        )}
                        {alert.prUrl && (
                          <a
                            href={alert.prUrl}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="inline-flex items-center gap-1 text-xs text-blue-400 hover:underline"
                          >
                            View PR
                            <ExternalLink className="h-3 w-3" />
                          </a>
                        )}
                        <span className="text-xs text-muted-foreground">
                          {formatRelativeTime(alert.timestamp)}
                        </span>
                      </div>
                      {alert.reasoning && (
                        <p className="mt-1 text-sm text-muted-foreground line-clamp-2">
                          {alert.reasoning}
                        </p>
                      )}
                    </div>
                  </div>
                </div>
              );
            })}
            {escalations.length > 5 && (
              <p className="text-center text-xs text-muted-foreground">
                +{escalations.length - 5} more alerts
              </p>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Stats Cards
// ---------------------------------------------------------------------------

function StatsCards({
  tasks,
  mergeQueue,
}: {
  tasks: Task[];
  mergeQueue: MergeQueueEntry[];
}) {
  const stats = useMemo(() => {
    const active = tasks.filter((t) =>
      ["running", "testing", "question"].includes(t.state)
    ).length;
    const pending = tasks.filter((t) =>
      ["waiting", "blocked"].includes(t.state)
    ).length;
    const awaitingMerge = tasks.filter(
      (t) => t.state === "awaiting_merge"
    ).length;
    const completed = tasks.filter((t) => t.state === "completed").length;
    const failed = tasks.filter((t) => t.state === "failed").length;
    const mergesPending = mergeQueue.filter(
      (e) => e.status === "pending"
    ).length;

    return { active, pending, awaitingMerge, completed, failed, mergesPending };
  }, [tasks, mergeQueue]);

  return (
    <div className="grid gap-4 md:grid-cols-3 lg:grid-cols-6">
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>Active</CardDescription>
          <CardTitle className="text-2xl">{stats.active}</CardTitle>
        </CardHeader>
      </Card>
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>Pending</CardDescription>
          <CardTitle className="text-2xl">{stats.pending}</CardTitle>
        </CardHeader>
      </Card>
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>Awaiting Merge</CardDescription>
          <CardTitle className="text-2xl">{stats.awaitingMerge}</CardTitle>
        </CardHeader>
      </Card>
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>Completed</CardDescription>
          <CardTitle className="text-2xl">{stats.completed}</CardTitle>
        </CardHeader>
      </Card>
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>Failed</CardDescription>
          <CardTitle className="text-2xl text-red-500">{stats.failed}</CardTitle>
        </CardHeader>
      </Card>
      <Card size="sm">
        <CardHeader className="pb-2">
          <CardDescription>PR Reviews</CardDescription>
          <CardTitle className="text-2xl">{stats.mergesPending}</CardTitle>
        </CardHeader>
      </Card>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Dashboard Page
// ---------------------------------------------------------------------------

export function DashboardPage() {
  const { filteredTasks, filteredMergeQueue, selectedProject, snapshot, events } =
    useAppState();
  const projects = snapshot?.projects ?? [];
  const tasks = snapshot?.tasks ?? [];

  const projectName = selectedProject
    ? projects.find((p) => p.id === selectedProject)?.repo ?? "Project"
    : "All Projects";

  return (
    <ScrollArea className="h-full">
      <div className="flex flex-col gap-6 p-6">
        {/* Header */}
        <div>
          <h1 className="text-lg font-semibold">{projectName}</h1>
          <p className="text-sm text-muted-foreground">
            Overview of current work and system status
          </p>
        </div>

        {/* Stats Cards */}
        <StatsCards tasks={filteredTasks} mergeQueue={filteredMergeQueue} />

        {/* Charts Row */}
        <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
          <Card>
            <CardHeader>
              <CardTitle>Task Status</CardTitle>
              <CardDescription>Distribution by state</CardDescription>
            </CardHeader>
            <CardContent>
              <TaskStateChart tasks={filteredTasks} />
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Task Breakdown</CardTitle>
              <CardDescription>Count by status</CardDescription>
            </CardHeader>
            <CardContent>
              <TaskStateBarChart tasks={filteredTasks} />
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Merge Queue</CardTitle>
              <CardDescription>PR review status</CardDescription>
            </CardHeader>
            <CardContent>
              <MergeQueueChart entries={filteredMergeQueue} />
            </CardContent>
          </Card>
        </div>

        {/* AI Summary and Alerts */}
        <div className="grid gap-6 md:grid-cols-2">
          <AISummary tasks={filteredTasks} mergeQueue={filteredMergeQueue} />
          <OrchestratorAlerts events={events} tasks={tasks} projects={projects} />
        </div>
      </div>
    </ScrollArea>
  );
}
