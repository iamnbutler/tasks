import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ArrowLeft,
  ExternalLink,
  Send,
  ChevronDown,
  ChevronRight,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { fetchTaskEvents, sendChat, subscribeEvents } from "@/lib/api";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
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

const stateStyles: Record<TaskState, string> = {
  waiting: "bg-muted text-muted-foreground",
  blocked: "bg-muted text-muted-foreground",
  running: "bg-blue-600 text-white",
  question: "bg-yellow-600 text-white",
  testing: "bg-purple-600 text-white",
  awaiting_merge: "bg-orange-500 text-white",
  conflict: "bg-red-600 text-white",
  completed: "bg-green-600 text-white",
  failed: "bg-red-600 text-white",
  cancelled: "bg-muted text-muted-foreground",
};

function stateBadge(state: TaskState) {
  const label = state === "awaiting_merge" ? "awaiting merge" : state;
  return <Badge className={stateStyles[state]}>{label}</Badge>;
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
        className="inline-flex items-center gap-1 text-blue-400 hover:underline"
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
        className="inline-flex items-center gap-1 text-blue-400 hover:underline"
      >
        {source.owner}/{source.repo}#{source.number} (PR)
        <ExternalLink className="h-3 w-3" />
      </a>
    );
  }
  return <span className="text-muted-foreground">Internal</span>;
}

// ---------------------------------------------------------------------------
// Session view — live agent output + chat
// ---------------------------------------------------------------------------

function SessionView({ taskId }: { taskId: string }) {
  const [messages, setMessages] = useState<Event[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [sending, setSending] = useState(false);
  const bottomRef = useRef<HTMLDivElement>(null);

  // Subscribe to live events for this task
  useEffect(() => {
    // Load historical events first
    fetchTaskEvents(taskId).then((events) => {
      const agentEvents = events.filter(
        (e) =>
          e.type === "agent:message" ||
          e.type === "agent:question" ||
          e.type === "agent:error"
      );
      setMessages(agentEvents.sort((a, b) => a.ts.localeCompare(b.ts)));
    });

    // Subscribe to live events
    const source = subscribeEvents({ task_id: taskId });
    source.onmessage = (msg) => {
      try {
        const event: Event = JSON.parse(msg.data);
        if (
          event.type === "agent:message" ||
          event.type === "agent:question" ||
          event.type === "agent:error"
        ) {
          setMessages((prev) => [...prev, event]);
        }
      } catch {
        // ignore
      }
    };
    return () => source.close();
  }, [taskId]);

  // Auto-scroll to bottom
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const handleSend = useCallback(async () => {
    const text = chatInput.trim();
    if (!text || sending) return;
    setSending(true);
    setChatInput("");
    try {
      await sendChat(taskId, text);
    } catch {
      // Message will show via SSE if it worked
    } finally {
      setSending(false);
    }
  }, [chatInput, sending, taskId]);

  return (
    <Card className="flex flex-col flex-1 min-h-0">
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">
          Session
        </CardTitle>
      </CardHeader>
      <CardContent className="flex flex-col flex-1 min-h-0 gap-3">
        {/* Message stream */}
        <div className="flex-1 min-h-0 overflow-y-auto rounded-md border border-border bg-muted/30 p-3 font-mono text-sm space-y-1">
          {messages.length === 0 && (
            <p className="text-muted-foreground text-center py-8">
              No agent output yet.
            </p>
          )}
          {messages.map((event) => {
            const content =
              typeof event.data?.message === "string"
                ? event.data.message
                : typeof event.data?.text === "string"
                  ? event.data.text
                  : JSON.stringify(event.data);
            return (
              <div
                key={event.id}
                className={cn(
                  "whitespace-pre-wrap break-words",
                  event.type === "agent:error" && "text-red-400",
                  event.type === "agent:question" && "text-yellow-400"
                )}
              >
                {content}
              </div>
            );
          })}
          <div ref={bottomRef} />
        </div>

        {/* Chat input */}
        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            handleSend();
          }}
        >
          <Input
            value={chatInput}
            onChange={(e) => setChatInput(e.target.value)}
            placeholder="Send a message to the agent..."
            className="flex-1"
            disabled={sending}
          />
          <Button type="submit" size="icon" disabled={sending || !chatInput.trim()}>
            <Send className="h-4 w-4" />
          </Button>
        </form>
      </CardContent>
    </Card>
  );
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
  const [showDetails, setShowDetails] = useState(false);

  const task = snapshot?.tasks.find((t) => t.id === id);

  const isSessionActive =
    task?.state === "running" ||
    task?.state === "question" ||
    task?.state === "testing";

  useEffect(() => {
    if (!id) return;
    setEventsLoading(true);
    fetchTaskEvents(id)
      .then((data) => {
        setEvents(
          data.sort(
            (a, b) => new Date(b.ts).getTime() - new Date(a.ts).getTime()
          )
        );
      })
      .catch(() => setEvents([]))
      .finally(() => setEventsLoading(false));
  }, [id]);

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

  return (
    <div className="flex flex-col h-full p-6 gap-4">
      {/* Header */}
      <div className="flex items-start gap-3 shrink-0">
        <Link to="/tasks">
          <Button variant="ghost" size="icon" className="shrink-0">
            <ArrowLeft className="h-4 w-4" />
          </Button>
        </Link>
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <h1 className="text-xl font-bold truncate">{task.title}</h1>
            {stateBadge(task.state)}
          </div>
          <div className="flex items-center gap-3 mt-1 text-sm text-muted-foreground">
            <span className="font-mono text-xs">{task.id.slice(0, 8)}</span>
            <Separator orientation="vertical" className="h-4" />
            {sourceDisplay(task)}
            <Separator orientation="vertical" className="h-4" />
            <span>{task.project}</span>
            {task.labels.length > 0 && (
              <>
                <Separator orientation="vertical" className="h-4" />
                <span className="flex gap-1">
                  {task.labels.map((l) => (
                    <Badge key={l} variant="outline" className="text-xs">
                      {l}
                    </Badge>
                  ))}
                </span>
              </>
            )}
          </div>
        </div>
      </div>

      {/* Session view — the main content when active */}
      {isSessionActive && id && (
        <SessionView taskId={id} />
      )}

      {/* Collapsible details */}
      <div className="shrink-0">
        <button
          onClick={() => setShowDetails(!showDetails)}
          className="flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground transition-colors"
        >
          {showDetails ? (
            <ChevronDown className="h-4 w-4" />
          ) : (
            <ChevronRight className="h-4 w-4" />
          )}
          Details
        </button>

        {showDetails && (
          <div className="mt-3 space-y-4">
            {/* Metadata */}
            <Card>
              <CardContent className="pt-4">
                <dl className="grid grid-cols-2 lg:grid-cols-4 gap-x-6 gap-y-3 text-sm">
                  <div>
                    <dt className="text-muted-foreground">Priority</dt>
                    <dd className="font-medium">
                      {task.priority !== null ? task.priority : "None"}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-muted-foreground">Created</dt>
                    <dd className="font-medium">
                      {formatRelativeTime(task.created_at)}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-muted-foreground">Updated</dt>
                    <dd className="font-medium">
                      {formatRelativeTime(task.updated_at)}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-muted-foreground">Retries</dt>
                    <dd className="font-medium">{task.retry_count}</dd>
                  </div>
                  {task.session_id && (
                    <div>
                      <dt className="text-muted-foreground">Session</dt>
                      <dd className="font-mono text-xs">
                        {task.session_id.slice(0, 12)}
                      </dd>
                    </div>
                  )}
                  {task.parent_id && (
                    <div>
                      <dt className="text-muted-foreground">Parent</dt>
                      <dd>
                        <Link
                          to={`/tasks/${task.parent_id}`}
                          className="text-blue-400 hover:underline font-mono text-xs"
                        >
                          {task.parent_id.slice(0, 8)}
                        </Link>
                      </dd>
                    </div>
                  )}
                  {task.blocked_by.length > 0 && (
                    <div>
                      <dt className="text-muted-foreground">Blocked by</dt>
                      <dd className="space-x-1">
                        {task.blocked_by.map((bid) => (
                          <Link
                            key={bid}
                            to={`/tasks/${bid}`}
                            className="text-blue-400 hover:underline font-mono text-xs"
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

            {/* Description */}
            {task.description && (
              <Card>
                <CardHeader className="pb-2">
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
          </div>
        )}
      </div>

      {/* Event timeline — always visible when no active session, or below details */}
      {(!isSessionActive || showDetails) && (
        <Card className="shrink-0">
          <CardHeader className="pb-2">
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
              <div className="overflow-x-auto max-h-96 overflow-y-auto">
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
      )}
    </div>
  );
}
