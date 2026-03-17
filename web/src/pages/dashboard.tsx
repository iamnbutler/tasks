import { useMemo } from "react";
import { Link } from "react-router-dom";
import { useAppState } from "@/hooks/use-app-state";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { formatRelativeTime } from "@/lib/utils";
import type { TaskState, Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// State badge styling
// ---------------------------------------------------------------------------

function stateBadge(state: TaskState) {
  switch (state) {
    case "running":
      return <Badge className="bg-blue-600 text-white">{state}</Badge>;
    case "question":
      return (
        <Badge variant="secondary" className="text-yellow-600">
          {state}
        </Badge>
      );
    case "testing":
      return <Badge variant="outline">{state}</Badge>;
    case "awaiting_merge":
      return <Badge variant="secondary">awaiting merge</Badge>;
    case "completed":
      return <Badge className="bg-green-600 text-white">{state}</Badge>;
    case "failed":
      return <Badge variant="destructive">{state}</Badge>;
    default:
      return <Badge variant="outline">{state}</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Event row helper
// ---------------------------------------------------------------------------

function eventDataPreview(data: Record<string, unknown>): string {
  const raw = JSON.stringify(data);
  return raw.length > 80 ? `${raw.slice(0, 80)}...` : raw;
}

// ---------------------------------------------------------------------------
// Dashboard page
// ---------------------------------------------------------------------------

const ACTIVE_STATES: TaskState[] = [
  "running",
  "question",
  "testing",
  "awaiting_merge",
];

export function DashboardPage() {
  const { snapshot, events, filteredTasks, filteredMergeQueue, selectedProject } = useAppState();

  const runningCount = useMemo(
    () => filteredTasks.filter((t) => t.state === "running").length,
    [filteredTasks],
  );

  const waitingCount = useMemo(
    () => filteredTasks.filter((t) => t.state === "waiting").length,
    [filteredTasks],
  );

  const activeTasks = useMemo(
    () =>
      filteredTasks
        .filter((t) => ACTIVE_STATES.includes(t.state))
        .sort(
          (a, b) =>
            new Date(b.updated_at).getTime() -
            new Date(a.updated_at).getTime(),
        )
        .slice(0, 10),
    [filteredTasks],
  );

  const recentEvents: Event[] = useMemo(
    () => events.slice(0, 15),
    [events],
  );

  // -----------------------------------------------------------------------
  // No data
  // -----------------------------------------------------------------------

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">No data yet</p>
      </div>
    );
  }

  // -----------------------------------------------------------------------
  // Render
  // -----------------------------------------------------------------------

  return (
    <div className="space-y-8 p-6">
      {/* Stats cards */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Active Sessions
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-base font-bold">
              {snapshot.slot_utilization.active}
              <span className="text-muted-foreground font-normal">
                {" "}
                / {snapshot.slot_utilization.max}
              </span>
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Running Tasks
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-base font-bold">{runningCount}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Waiting Tasks
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-base font-bold">{waitingCount}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Merge Queue
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-base font-bold">{filteredMergeQueue.length}</p>
          </CardContent>
        </Card>
      </div>

      {/* Active Tasks */}
      <section>
        <h2 className="text-base font-semibold mb-3">Active Tasks</h2>

        {activeTasks.length === 0 ? (
          <p className="text-muted-foreground text-sm">
            No active tasks right now.
          </p>
        ) : (
          <div className="space-y-2">
            {activeTasks.map((task) => (
              <Link
                key={task.id}
                to={`/tasks/${task.id}`}
                className="block rounded-lg border bg-card p-3 hover:bg-accent/50 transition-colors"
              >
                <div className="flex items-center gap-3">
                  {stateBadge(task.state)}
                  <span className="font-medium truncate flex-1">
                    {task.title}
                  </span>
                  {/* Only show project when viewing all projects */}
                  {!selectedProject && (
                    <span className="text-muted-foreground text-sm shrink-0">
                      {task.project}
                    </span>
                  )}
                  <span className="text-muted-foreground text-sm shrink-0">
                    {formatRelativeTime(task.updated_at)}
                  </span>
                </div>
              </Link>
            ))}
          </div>
        )}
      </section>

      {/* Recent Events */}
      <section>
        <h2 className="text-base font-semibold mb-3">Recent Events</h2>

        {recentEvents.length === 0 ? (
          <p className="text-muted-foreground text-sm">No events yet.</p>
        ) : (
          <div className="space-y-1">
            {recentEvents.map((event) => (
              <div
                key={event.id}
                className="flex items-start gap-3 rounded-lg border bg-card px-3 py-2 text-sm"
              >
                <span className="text-muted-foreground text-sm whitespace-nowrap pt-0.5">
                  {formatRelativeTime(event.ts)}
                </span>
                <Badge variant="outline" className="shrink-0">
                  {event.type}
                </Badge>
                <span className="text-muted-foreground text-sm shrink-0">
                  {event.actor}
                </span>
                {event.task && (
                  <span className="font-mono text-sm text-muted-foreground shrink-0">
                    {event.task.slice(0, 8)}
                  </span>
                )}
                <span className="text-sm text-muted-foreground truncate flex-1">
                  {eventDataPreview(event.data)}
                </span>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
