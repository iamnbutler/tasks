import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ArrowLeft,
  ExternalLink,
  Send,
  ChevronDown,
  ChevronRight,
  FileText,
  Terminal,
  StopCircle,
} from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useAppState } from "@/hooks/use-app-state";
import { cancelTask, fetchTaskEvents, sendChat, subscribeEvents } from "@/lib/api";
import { cn, formatRelativeTime, projectLabel } from "@/lib/utils";
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
// Parse Claude Code protocol messages from agent:message events.
//
// Event data shape: { text: "<raw JSON line from agent stdout>" }
// The JSON line is a Claude Code protocol message with structure:
//   { type: "system"|"assistant"|"user"|"result", message?: { content: [...] }, ... }
// Content blocks: { type: "text", text }, { type: "tool_use", name, input },
//   { type: "tool_result", content }, { type: "thinking", thinking }
// ---------------------------------------------------------------------------

interface ParsedBlock {
  kind: "text" | "thinking" | "tool_use" | "tool_result" | "error" | "system" | "lifecycle";
  content: string;
  toolName?: string;
  filePath?: string;
}

/** Human-readable labels for task lifecycle events. */
const lifecycleLabels: Record<string, string> = {
  "task:created": "Task created",
  "task:state:running": "Agent started",
  "task:state:question": "Agent has a question",
  "task:state:waiting": "Task waiting",
  "task:state:blocked": "Task blocked",
  "task:state:testing": "Running tests",
  "task:state:awaiting_merge": "Awaiting merge",
  "task:state:conflict": "Merge conflict detected",
  "task:state:completed": "Task completed",
  "task:state:failed": "Task failed",
  "task:state:cancelled": "Task cancelled",
};

function parseAgentEvents(events: Event[]): ParsedBlock[] {
  const blocks: ParsedBlock[] = [];

  for (const event of events) {
    // Lifecycle events — task state transitions, container pickup, etc.
    if (event.type.startsWith("task:")) {
      const label = lifecycleLabels[event.type];
      if (label) {
        blocks.push({ kind: "lifecycle", content: label });
      }
      continue;
    }

    if (event.type === "agent:error") {
      blocks.push({
        kind: "error",
        content: typeof event.data?.text === "string" ? event.data.text : JSON.stringify(event.data),
      });
      continue;
    }

    // event.data.text is a raw JSON line from Claude Code stdout
    const raw = event.data?.text;
    if (typeof raw !== "string") continue;

    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw);
    } catch {
      // Not JSON — show as plain text
      if (raw.trim()) blocks.push({ kind: "text", content: raw });
      continue;
    }

    // Skip system init messages
    if (msg.type === "system") continue;

    // result messages (final summary)
    if (msg.type === "result") {
      const result = msg.result as Record<string, unknown> | undefined;
      if (typeof result?.text === "string" && result.text) {
        blocks.push({ kind: "text", content: result.text });
      }
      continue;
    }

    // assistant and user messages — extract content blocks
    const message = msg.message as Record<string, unknown> | undefined;
    const contentBlocks = (message?.content ?? msg.content) as unknown[] | undefined;
    if (!Array.isArray(contentBlocks)) continue;

    for (const block of contentBlocks) {
      if (typeof block !== "object" || block === null) continue;
      const b = block as Record<string, unknown>;

      if (b.type === "thinking" && typeof b.thinking === "string") {
        // Skip thinking blocks — they're internal reasoning
        continue;
      }

      if (b.type === "text" && typeof b.text === "string") {
        if (b.text.trim()) {
          blocks.push({ kind: "text", content: b.text });
        }
        continue;
      }

      if (b.type === "tool_use") {
        const name = typeof b.name === "string" ? b.name : "tool";
        const input = (b.input ?? {}) as Record<string, unknown>;
        const filePath = input.file_path ?? input.filePath ?? input.path ?? input.pattern;
        const command = input.command;
        const description = input.description;
        // Show the most useful piece of context
        const detail = filePath ?? command ?? description;
        blocks.push({
          kind: "tool_use",
          content: detail ? String(detail) : "",
          toolName: name,
          filePath: filePath ? String(filePath) : undefined,
        });
        continue;
      }

      if (b.type === "tool_result") {
        const content = typeof b.content === "string" ? b.content : "";
        if (!content) continue;
        // Truncate large results
        const lines = content.split("\n");
        const preview =
          lines.length > 30
            ? lines.slice(0, 25).join("\n") + `\n... (${lines.length} total lines)`
            : content;
        blocks.push({ kind: "tool_result", content: preview });
        continue;
      }
    }
  }

  return blocks;
}

// ---------------------------------------------------------------------------
// Rendered block components
// ---------------------------------------------------------------------------

function BlockView({ block }: { block: ParsedBlock }) {
  if (block.kind === "tool_use") {
    return (
      <div className="flex items-center gap-2 py-1 text-muted-foreground text-sm">
        <Terminal className="h-3 w-3 shrink-0" />
        <span className="font-medium">{block.toolName}</span>
        {block.content && (
          <span className="font-mono truncate">{block.content}</span>
        )}
      </div>
    );
  }

  if (block.kind === "tool_result") {
    const [open, setOpen] = useState(false);
    const isLong = block.content.split("\n").length > 5;
    return (
      <div className="rounded-md border border-border bg-muted/50 text-sm overflow-hidden">
        <button
          onClick={() => setOpen(!open)}
          className="flex items-center gap-2 px-3 py-1.5 w-full text-left text-muted-foreground text-sm hover:bg-muted/80"
        >
          <FileText className="h-3 w-3 shrink-0" />
          <span>Output</span>
          {isLong && (
            open
              ? <ChevronDown className="h-3 w-3 ml-auto" />
              : <ChevronRight className="h-3 w-3 ml-auto" />
          )}
        </button>
        {(open || !isLong) && (
          <pre className="px-3 py-2 text-sm font-mono overflow-x-auto whitespace-pre-wrap max-h-64 overflow-y-auto text-muted-foreground border-t border-border">
            {block.content}
          </pre>
        )}
      </div>
    );
  }

  if (block.kind === "lifecycle") {
    return (
      <div className="flex items-center gap-2 py-1.5 text-sm text-muted-foreground">
        <div className="h-px flex-1 bg-border" />
        <span>{block.content}</span>
        <div className="h-px flex-1 bg-border" />
      </div>
    );
  }

  if (block.kind === "error") {
    return (
      <div className="rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-400">
        {block.content}
      </div>
    );
  }

  // text — render as markdown
  return (
    <div className="prose prose-sm prose-invert max-w-none">
      <Markdown remarkPlugins={[remarkGfm]}>{block.content}</Markdown>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Session view — live agent output + chat
// ---------------------------------------------------------------------------

function SessionView({ taskId, chatEnabled }: { taskId: string; chatEnabled: boolean }) {
  const [rawEvents, setRawEvents] = useState<Event[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [sending, setSending] = useState(false);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const isRelevant = (e: Event) =>
      e.type === "agent:message" ||
      e.type === "agent:question" ||
      e.type === "agent:error" ||
      e.type.startsWith("task:");

    fetchTaskEvents(taskId).then((events) => {
      setRawEvents(
        events.filter(isRelevant).sort((a, b) => a.ts.localeCompare(b.ts))
      );
    });

    const source = subscribeEvents({ task_id: taskId });
    source.onmessage = (msg) => {
      try {
        const event: Event = JSON.parse(msg.data);
        if (isRelevant(event)) {
          setRawEvents((prev) => [...prev, event]);
        }
      } catch {
        // ignore
      }
    };
    return () => source.close();
  }, [taskId]);

  const blocks = parseAgentEvents(rawEvents);
  const prevBlocksLength = useRef(blocks.length);

  // Check if user is scrolled to bottom (within threshold)
  const checkIfAtBottom = useCallback(() => {
    const container = scrollContainerRef.current;
    if (!container) return true;
    const threshold = 50;
    return container.scrollHeight - container.scrollTop - container.clientHeight <= threshold;
  }, []);

  // Handle scroll events to track position
  const handleScroll = useCallback(() => {
    const atBottom = checkIfAtBottom();
    setIsAtBottom(atBottom);
    if (atBottom) {
      setHasNewMessages(false);
    }
  }, [checkIfAtBottom]);

  // Auto-scroll to bottom only if user was at bottom, otherwise show indicator
  useEffect(() => {
    if (blocks.length > prevBlocksLength.current) {
      if (isAtBottom) {
        bottomRef.current?.scrollIntoView({ behavior: "smooth" });
      } else {
        setHasNewMessages(true);
      }
    }
    prevBlocksLength.current = blocks.length;
  }, [blocks.length, isAtBottom]);

  // Scroll to bottom when clicking the new messages indicator
  const scrollToBottom = useCallback(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    setHasNewMessages(false);
    setIsAtBottom(true);
  }, []);

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
        <div className="relative flex-1 min-h-0">
          <div
            ref={scrollContainerRef}
            onScroll={handleScroll}
            className="absolute inset-0 overflow-y-auto rounded-md border border-border bg-background p-4 space-y-3"
          >
            {blocks.length === 0 && (
              <p className="text-muted-foreground text-center py-8">
                No agent output yet.
              </p>
            )}
            {blocks.map((block, i) => (
              <BlockView key={i} block={block} />
            ))}
            <div ref={bottomRef} />
          </div>
          {/* New messages indicator */}
          {hasNewMessages && (
            <button
              onClick={scrollToBottom}
              className="absolute bottom-3 left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 rounded-full bg-blue-600 text-white text-sm font-medium shadow-lg hover:bg-blue-700 transition-colors"
            >
              <ChevronDown className="h-4 w-4" />
              New messages
            </button>
          )}
        </div>

        {/* Chat input — only when session is active */}
        {chatEnabled && (
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
        )}
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
  const [cancelling, setCancelling] = useState(false);

  const task = snapshot?.tasks.find((t) => t.id === id);

  const isSessionActive =
    task?.state === "running" ||
    task?.state === "question" ||
    task?.state === "testing";

  const handleCancel = useCallback(async () => {
    if (!id || cancelling) return;
    setCancelling(true);
    try {
      await cancelTask(id);
    } catch {
      // Error will be reflected in task state change via SSE
    } finally {
      setCancelling(false);
    }
  }, [id, cancelling]);

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
        <p className="text-muted-foreground text-sm">Loading...</p>
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
            <h1 className="text-base font-bold truncate">{task.title}</h1>
            {stateBadge(task.state)}
            {isSessionActive && (
              <Button
                variant="destructive"
                size="sm"
                onClick={handleCancel}
                disabled={cancelling}
                className="ml-2 gap-1"
              >
                <StopCircle className="h-4 w-4" />
                {cancelling ? "Cancelling..." : "Cancel"}
              </Button>
            )}
          </div>
          <div className="flex items-center gap-3 mt-1 text-sm text-muted-foreground">
            <span className="font-mono text-sm">{task.id.slice(0, 8)}</span>
            <Separator orientation="vertical" className="h-4" />
            {sourceDisplay(task)}
            <Separator orientation="vertical" className="h-4" />
            <span>{projectLabel(task.project, snapshot.projects)}</span>
            {task.labels.length > 0 && (
              <>
                <Separator orientation="vertical" className="h-4" />
                <span className="flex gap-1">
                  {task.labels.map((l) => (
                    <Badge key={l} variant="outline">
                      {l}
                    </Badge>
                  ))}
                </span>
              </>
            )}
          </div>
        </div>
      </div>

      {/* Session view — always shown, chat input only when active */}
      {id && (
        <SessionView taskId={id} chatEnabled={isSessionActive} />
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
                      <dd className="font-mono text-sm">
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
                          className="text-blue-400 hover:underline font-mono text-sm"
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
                            className="text-blue-400 hover:underline font-mono text-sm"
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

      {/* Raw event timeline — only in details view */}
      {showDetails && (
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
                        <TableCell className="text-sm text-muted-foreground whitespace-nowrap">
                          {formatRelativeTime(event.ts)}
                        </TableCell>
                        <TableCell>
                          <Badge variant="outline">
                            {event.type}
                          </Badge>
                        </TableCell>
                        <TableCell className="text-sm text-muted-foreground">
                          {event.actor}
                        </TableCell>
                        <TableCell className="text-sm font-mono text-muted-foreground max-w-md truncate">
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
