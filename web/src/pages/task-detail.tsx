import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { ArrowLeft, ExternalLink } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { fetchTaskEvents } from "@/lib/api";
import { formatRelativeTime } from "@/lib/utils";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { Event, Task, TaskState } from "@/lib/types";

// ---------------------------------------------------------------------------
// State badge
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
    case "conflict":
      return <Badge className="bg-orange-600 text-white">{state}</Badge>;
    case "cancelled":
      return <Badge variant="secondary">{state}</Badge>;
    default:
      return <Badge variant="outline">{state}</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Source link helper
// ---------------------------------------------------------------------------

function sourceDisplay(task: Task) {
  const { source } = task;
  if (source.type === "github_issue") {
    const url = `https://github.com/${source.owner}/${source.repo}/issues/${source.number}`;
    return (
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        className="inline-flex items-center gap-1 text-blue-600 hover:underline"
      >
        {source.owner}/{source.repo}#{source.number}
        <ExternalLink className="h-3 w-3" />
      </a>
    );
  }
  if (source.type === "github_pr") {
    const url = `https://github.com/${source.owner}/${source.repo}/pull/${source.number}`;
    return (
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        className="inline-flex items-center gap-1 text-blue-600 hover:underline"
      >
        {source.owner}/{source.repo}#{source.number} (PR)
        <ExternalLink className="h-3 w-3" />
      </a>
    );
  }
  return <span className="text-muted-foreground">Internal</span>;
}

// ---------------------------------------------------------------------------
// Event data preview
// ---------------------------------------------------------------------------

function eventDataPreview(data: Record<string, unknown>): string {
  const raw = JSON.stringify(data);
  return raw.length > 120 ? `${raw.slice(0, 120)}...` : raw;
}

// ---------------------------------------------------------------------------
// Task Detail Page
// ---------------------------------------------------------------------------

export function TaskDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { snapshot } = useAppState();
  const [events, setEvents] = useState<Event[]>([]);
  const [eventsLoading, setEventsLoading] = useState(true);

  const task = snapshot?.tasks.find((t) => t.id === id);

  useEffect(() => {
    if (!id) return;
    setEventsLoading(true);
    fetchTaskEvents(id)
      .then((data) => {
        setEvents(data.sort((a, b) => new Date(b.ts).getTime() - new Date(a.ts).getTime()));
      })
      .catch(() => {
        setEvents([]);
      })
      .finally(() => {
        setEventsLoading(false);
      });
  }, [id]);

  // -------------------------------------------------------------------------
  // Not found
  // -------------------------------------------------------------------------

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-lg">Loading...</p>
      </div>
    );
  }

  if (!task) {
    return (
      <div className="space-y-4 p-6">
        <Link to="/tasks">
          <Button variant="ghost" size="sm" className="gap-1">
            <ArrowLeft className="h-4 w-4" />
            Back to Tasks
          </Button>
        </Link>
        <p className="text-muted-foreground">Task not found.</p>
      </div>
    );
  }

  // -------------------------------------------------------------------------
  // Render
  // -------------------------------------------------------------------------

  return (
    <div className="space-y-6 p-6">
      {/* Back link */}
      <Link to="/tasks">
        <Button variant="ghost" size="sm" className="gap-1">
          <ArrowLeft className="h-4 w-4" />
          Back to Tasks
        </Button>
      </Link>

      {/* Title + state */}
      <div className="flex items-start gap-3">
        <div className="space-y-1 flex-1">
          <h1 className="text-2xl font-bold">{task.title}</h1>
          <p className="text-sm text-muted-foreground font-mono">{task.id}</p>
        </div>
        {stateBadge(task.state)}
      </div>

      {/* Metadata grid */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm font-medium text-muted-foreground">
            Details
          </CardTitle>
        </CardHeader>
        <CardContent>
          <dl className="grid grid-cols-1 gap-x-6 gap-y-3 sm:grid-cols-2 lg:grid-cols-3 text-sm">
            <div>
              <dt className="text-muted-foreground">Source</dt>
              <dd className="mt-0.5 font-medium">{sourceDisplay(task)}</dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Project</dt>
              <dd className="mt-0.5 font-medium">{task.project}</dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Priority</dt>
              <dd className="mt-0.5 font-medium">
                {task.priority !== null ? task.priority : "None"}
              </dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Created</dt>
              <dd className="mt-0.5 font-medium">
                {formatRelativeTime(task.created_at)}
              </dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Updated</dt>
              <dd className="mt-0.5 font-medium">
                {formatRelativeTime(task.updated_at)}
              </dd>
            </div>
            <div>
              <dt className="text-muted-foreground">Retry Count</dt>
              <dd className="mt-0.5 font-medium">{task.retry_count}</dd>
            </div>
            {task.session_id && (
              <div>
                <dt className="text-muted-foreground">Session ID</dt>
                <dd className="mt-0.5 font-mono text-xs">{task.session_id}</dd>
              </div>
            )}
            {task.parent_id && (
              <div>
                <dt className="text-muted-foreground">Parent Task</dt>
                <dd className="mt-0.5">
                  <Link
                    to={`/tasks/${task.parent_id}`}
                    className="text-blue-600 hover:underline font-mono text-xs"
                  >
                    {task.parent_id.slice(0, 8)}...
                  </Link>
                </dd>
              </div>
            )}
            {task.blocked_by.length > 0 && (
              <div>
                <dt className="text-muted-foreground">Blocked By</dt>
                <dd className="mt-0.5 space-x-1">
                  {task.blocked_by.map((bid) => (
                    <Link
                      key={bid}
                      to={`/tasks/${bid}`}
                      className="text-blue-600 hover:underline font-mono text-xs"
                    >
                      {bid.slice(0, 8)}
                    </Link>
                  ))}
                </dd>
              </div>
            )}
          </dl>
        </CardContent>
      </Card>

      {/* Labels */}
      {task.labels.length > 0 && (
        <div className="flex items-center gap-2 flex-wrap">
          <span className="text-sm text-muted-foreground">Labels:</span>
          {task.labels.map((label) => (
            <Badge key={label} variant="outline">
              {label}
            </Badge>
          ))}
        </div>
      )}

      {/* Description */}
      {task.description && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Description
            </CardTitle>
          </CardHeader>
          <CardContent>
            <pre className="whitespace-pre-wrap text-sm font-mono bg-muted/50 rounded-md p-4 overflow-x-auto">
              {task.description}
            </pre>
          </CardContent>
        </Card>
      )}

      {/* Event timeline */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm font-medium text-muted-foreground">
            Event Timeline
          </CardTitle>
        </CardHeader>
        <CardContent>
          {eventsLoading ? (
            <p className="text-sm text-muted-foreground">Loading events...</p>
          ) : events.length === 0 ? (
            <p className="text-sm text-muted-foreground">No events found.</p>
          ) : (
            <div className="overflow-x-auto">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-[100px]">Time</TableHead>
                    <TableHead className="w-[160px]">Type</TableHead>
                    <TableHead className="w-[100px]">Actor</TableHead>
                    <TableHead>Data</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {events.map((event) => (
                    <TableRow key={event.id}>
                      <TableCell className="text-xs text-muted-foreground whitespace-nowrap">
                        {formatRelativeTime(event.ts)}
                      </TableCell>
                      <TableCell>
                        <Badge variant="outline" className="text-xs">
                          {event.type}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-xs text-muted-foreground">
                        {event.actor}
                      </TableCell>
                      <TableCell className="text-xs font-mono text-muted-foreground max-w-md truncate">
                        {eventDataPreview(event.data)}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
