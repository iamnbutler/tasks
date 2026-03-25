import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ExternalLink,
  Send,
  ChevronDown,
  ChevronRight,
  FileText,
  Terminal,
  StopCircle,
  User,
  Bot,
  RotateCcw,
  Clock,
  Activity,
  MessageSquare,
  HelpCircle,
  Brain,
} from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useAppState } from "@/hooks/use-app-state";
import { cancelTask, fetchTaskEvents, sendChat, subscribeEvents } from "@/lib/api";
import { cn, formatRelativeTime, projectLabel } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { Event, FailureInfo, Task, TaskState } from "@/lib/types";
import { taskStateMeta } from "./tasks/columns";

// ---------------------------------------------------------------------------
// State badge
// ---------------------------------------------------------------------------

function StateBadge({ state }: { state: TaskState }) {
  const meta = taskStateMeta[state];
  if (!meta) return null;
  const Icon = meta.icon;
  return (
    <Badge variant="outline" className={cn("gap-1", meta.className)}>
      <Icon className="h-3.5 w-3.5" />
      {meta.label}
    </Badge>
  );
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
        className="inline-flex items-center gap-1 text-blue-400 hover:underline text-sm"
      >
        #{source.number}
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
        className="inline-flex items-center gap-1 text-blue-400 hover:underline text-sm"
      >
        #{source.number} (PR)
        <ExternalLink className="h-3 w-3" />
      </a>
    );
  }
  return <span className="text-muted-foreground text-sm">Internal</span>;
}

// ---------------------------------------------------------------------------
// Parse Claude Code protocol messages
// ---------------------------------------------------------------------------

interface ParsedBlock {
  kind: "text" | "thinking" | "tool_use" | "tool_result" | "error" | "system" | "lifecycle" | "human_message" | "agent_question" | "orchestrator_answer" | "session_boundary";
  content: string;
  toolName?: string;
  filePath?: string;
  timestamp?: string;
}

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
  let runningCount = 0;

  for (const event of events) {
    if (event.type === "task:state:running") {
      runningCount++;
      if (runningCount > 1) {
        blocks.push({
          kind: "session_boundary",
          content: "New session started (retry)",
          timestamp: event.ts,
        });
      }
    }

    if (event.type.startsWith("task:")) {
      const label = lifecycleLabels[event.type];
      if (label) {
        blocks.push({ kind: "lifecycle", content: label, timestamp: event.ts });
      }
      continue;
    }

    if (event.type === "agent:question") {
      const question = (event.data?.question ?? event.data?.message ?? event.data?.text) as string | undefined;
      if (question) {
        blocks.push({ kind: "agent_question", content: question, timestamp: event.ts });
      }
      continue;
    }

    if (event.type === "human:message") {
      const message = event.data?.message as string | undefined;
      const source = event.data?.source as string | undefined;
      if (message) {
        // Orchestrator answers come as human:message with source "orchestrator_answer"
        blocks.push({
          kind: source === "orchestrator_answer" ? "orchestrator_answer" : "human_message",
          content: message,
          timestamp: event.ts,
        });
      }
      continue;
    }

    if (event.type === "agent:error") {
      blocks.push({
        kind: "error",
        content: typeof event.data?.text === "string" ? event.data.text : JSON.stringify(event.data),
        timestamp: event.ts,
      });
      continue;
    }

    const raw = event.data?.text;
    if (typeof raw !== "string") continue;

    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw);
    } catch {
      if (raw.trim()) blocks.push({ kind: "text", content: raw });
      continue;
    }

    if (msg.type === "system") continue;

    if (msg.type === "result") {
      const result = msg.result as Record<string, unknown> | undefined;
      if (typeof result?.text === "string" && result.text) {
        blocks.push({ kind: "text", content: result.text });
      }
      continue;
    }

    const message = msg.message as Record<string, unknown> | undefined;
    const contentBlocks = (message?.content ?? msg.content) as unknown[] | undefined;
    if (!Array.isArray(contentBlocks)) continue;

    for (const block of contentBlocks) {
      if (typeof block !== "object" || block === null) continue;
      const b = block as Record<string, unknown>;

      if (b.type === "thinking") continue;

      if (b.type === "text" && typeof b.text === "string") {
        if (b.text.trim()) {
          blocks.push({ kind: "text", content: b.text, timestamp: event.ts });
        }
        continue;
      }

      if (b.type === "tool_use") {
        const name = typeof b.name === "string" ? b.name : "tool";
        const input = (b.input ?? {}) as Record<string, unknown>;
        const filePath = input.file_path ?? input.filePath ?? input.path ?? input.pattern;
        const command = input.command;
        const description = input.description;
        const detail = filePath ?? command ?? description;
        blocks.push({
          kind: "tool_use",
          content: detail ? String(detail) : "",
          toolName: name,
          filePath: filePath ? String(filePath) : undefined,
          timestamp: event.ts,
        });
        continue;
      }

      if (b.type === "tool_result") {
        const content = typeof b.content === "string" ? b.content : "";
        if (!content) continue;
        const lines = content.split("\n");
        const preview =
          lines.length > 30
            ? lines.slice(0, 25).join("\n") + `\n... (${lines.length} total lines)`
            : content;
        blocks.push({ kind: "tool_result", content: preview, timestamp: event.ts });
        continue;
      }
    }
  }

  return blocks;
}

// ---------------------------------------------------------------------------
// Block view components
// ---------------------------------------------------------------------------

function formatMessageTime(ts?: string): string | null {
  if (!ts) return null;
  try {
    return new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return null;
  }
}

function ToolResultBlock({ block }: { block: ParsedBlock }) {
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
          open ? <ChevronDown className="h-3 w-3 ml-auto" /> : <ChevronRight className="h-3 w-3 ml-auto" />
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

function BlockView({ block }: { block: ParsedBlock }) {
  const timestamp = formatMessageTime(block.timestamp);

  if (block.kind === "human_message") {
    return (
      <div className="flex justify-end">
        <div className="max-w-[80%] flex flex-col items-end gap-1">
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <span>You</span>
            <User className="h-3 w-3" />
          </div>
          <div className="rounded-lg bg-blue-600 px-3 py-2 text-sm text-white">
            {block.content}
          </div>
          {timestamp && <span className="text-xs text-muted-foreground">{timestamp}</span>}
        </div>
      </div>
    );
  }

  if (block.kind === "agent_question") {
    return (
      <div className="rounded-md border border-violet-500/30 bg-violet-500/10 px-3 py-2">
        <div className="flex items-center gap-2 text-xs text-violet-400 mb-1">
          <HelpCircle className="h-3 w-3" />
          <span className="font-medium">Agent is asking a question</span>
          {timestamp && <span className="text-muted-foreground ml-auto">{timestamp}</span>}
        </div>
        <p className="text-sm text-violet-300">{block.content}</p>
      </div>
    );
  }

  if (block.kind === "orchestrator_answer") {
    return (
      <div className="rounded-md border border-orange-500/30 bg-orange-500/10 px-3 py-2">
        <div className="flex items-center gap-2 text-xs text-orange-400 mb-1">
          <Brain className="h-3 w-3" />
          <span className="font-medium">Orchestrator answered</span>
          {timestamp && <span className="text-muted-foreground ml-auto">{timestamp}</span>}
        </div>
        <p className="text-sm text-orange-300/90 whitespace-pre-wrap">{block.content}</p>
      </div>
    );
  }

  if (block.kind === "session_boundary") {
    return (
      <div className="flex items-center gap-2 py-3 text-sm text-yellow-500">
        <div className="h-px flex-1 bg-yellow-500/30" />
        <RotateCcw className="h-4 w-4" />
        <span className="font-medium">{block.content}</span>
        <div className="h-px flex-1 bg-yellow-500/30" />
      </div>
    );
  }

  if (block.kind === "tool_use") {
    return (
      <div className="flex items-center gap-2 py-1 text-muted-foreground text-sm">
        <Terminal className="h-3 w-3 shrink-0" />
        <span className="font-medium">{block.toolName}</span>
        {block.content && <span className="font-mono truncate">{block.content}</span>}
      </div>
    );
  }

  if (block.kind === "tool_result") {
    return <ToolResultBlock block={block} />;
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

  return (
    <div className="flex gap-2">
      <div className="shrink-0 mt-1">
        <div className="flex items-center justify-center h-6 w-6 rounded-full bg-muted">
          <Bot className="h-3.5 w-3.5 text-muted-foreground" />
        </div>
      </div>
      <div className="flex-1 min-w-0">
        <div className="prose prose-sm prose-invert max-w-none">
          <Markdown remarkPlugins={[remarkGfm]}>{block.content}</Markdown>
        </div>
        {timestamp && <span className="text-xs text-muted-foreground mt-1 block">{timestamp}</span>}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Session view
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
      e.type === "human:message" ||
      e.type === "orchestrator:feedback" ||
      e.type.startsWith("task:");

    fetchTaskEvents(taskId).then((events) => {
      setRawEvents(events.filter(isRelevant).sort((a, b) => a.ts.localeCompare(b.ts)));
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

  const checkIfAtBottom = useCallback(() => {
    const container = scrollContainerRef.current;
    if (!container) return true;
    return container.scrollHeight - container.scrollTop - container.clientHeight <= 50;
  }, []);

  const handleScroll = useCallback(() => {
    const atBottom = checkIfAtBottom();
    setIsAtBottom(atBottom);
    if (atBottom) setHasNewMessages(false);
  }, [checkIfAtBottom]);

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
    <div className="flex flex-col flex-1 min-h-0">
      {/* Message stream */}
      <div className="relative flex-1 min-h-0">
        <div
          ref={scrollContainerRef}
          onScroll={handleScroll}
          className="absolute inset-0 overflow-y-auto p-4 space-y-2"
        >
          {blocks.length === 0 && (
            <p className="text-muted-foreground text-center py-8 text-sm">
              No agent output yet.
            </p>
          )}
          {blocks.map((block, i) => (
            <BlockView key={i} block={block} />
          ))}
          <div ref={bottomRef} />
        </div>
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

      {/* Chat input */}
      {chatEnabled && (
        <form
          className="flex gap-2 p-3 border-t border-border"
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
    </div>
  );
}

// ---------------------------------------------------------------------------
// Failure info
// ---------------------------------------------------------------------------

function FailureInfoSection({ failure }: { failure: FailureInfo }) {
  const [stderrExpanded, setStderrExpanded] = useState(false);
  const hasStderr = failure.stderr_tail.length > 0;

  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <Badge
          variant="outline"
          className={cn(
            failure.failure_type === "transient"
              ? "border-yellow-500/50 text-yellow-400"
              : "border-red-500/50 text-red-400"
          )}
        >
          {failure.failure_type}
        </Badge>
        <span className="text-xs text-muted-foreground">{failure.duration_secs}s</span>
      </div>
      <p className="text-xs text-muted-foreground">{failure.summary}</p>
      {failure.exit_code !== null && (
        <div className="text-xs text-muted-foreground">
          Exit code: <span className="font-mono">{failure.exit_code}</span>
        </div>
      )}
      {hasStderr && (
        <div>
          <button
            onClick={() => setStderrExpanded(!stderrExpanded)}
            className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
          >
            {stderrExpanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
            <Terminal className="h-3 w-3" />
            Stderr ({failure.stderr_tail.length} lines)
          </button>
          {stderrExpanded && (
            <pre className="mt-1 text-xs font-mono bg-muted/50 rounded-md p-2 overflow-x-auto max-h-32 overflow-y-auto text-red-300/80">
              {failure.stderr_tail.join("\n")}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Event timeline
// ---------------------------------------------------------------------------

interface TimelineEvent {
  id: string;
  type: string;
  ts: string;
  actor: string;
  description: string;
}

function parseTimelineEvents(events: Event[]): TimelineEvent[] {
  const timelineEvents: TimelineEvent[] = [];

  for (const event of events) {
    // Only include lifecycle/state events in timeline
    if (event.type.startsWith("task:")) {
      const label = lifecycleLabels[event.type];
      if (label) {
        timelineEvents.push({
          id: event.id,
          type: event.type,
          ts: event.ts,
          actor: event.actor,
          description: label,
        });
      }
    }
  }

  return timelineEvents.sort((a, b) => a.ts.localeCompare(b.ts));
}

function formatTimelineTime(ts: string): string {
  try {
    const date = new Date(ts);
    return date.toLocaleString([], {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });
  } catch {
    return ts;
  }
}

function EventTimeline({ taskId }: { taskId: string }) {
  const [events, setEvents] = useState<Event[]>([]);
  const [isOpen, setIsOpen] = useState(false);

  useEffect(() => {
    fetchTaskEvents(taskId).then((allEvents) => {
      setEvents(allEvents.sort((a, b) => a.ts.localeCompare(b.ts)));
    });

    const source = subscribeEvents({ task_id: taskId });
    source.onmessage = (msg) => {
      try {
        const event: Event = JSON.parse(msg.data);
        setEvents((prev) => [...prev, event]);
      } catch {
        // ignore
      }
    };
    return () => source.close();
  }, [taskId]);

  const timelineEvents = parseTimelineEvents(events);

  if (timelineEvents.length === 0) {
    return null;
  }

  return (
    <Collapsible open={isOpen} onOpenChange={setIsOpen}>
      <CollapsibleTrigger className="flex items-center gap-2 w-full py-2 text-left hover:bg-muted/50 rounded-md px-2 -mx-2 transition-colors">
        {isOpen ? (
          <ChevronDown className="h-4 w-4 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-4 w-4 text-muted-foreground" />
        )}
        <Activity className="h-4 w-4 text-muted-foreground" />
        <span className="text-sm font-medium">Event Timeline</span>
        <Badge variant="outline" className="ml-auto text-xs">
          {timelineEvents.length}
        </Badge>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="mt-2 ml-1 border-l border-border pl-4 space-y-3">
          {timelineEvents.map((event, idx) => (
            <div key={event.id} className="relative">
              {/* Timeline dot */}
              <div className="absolute -left-[21px] top-1.5 h-2 w-2 rounded-full bg-muted-foreground/50" />
              <div className="flex flex-col gap-0.5">
                <span className="text-sm">{event.description}</span>
                <div className="flex items-center gap-2 text-xs text-muted-foreground">
                  <Clock className="h-3 w-3" />
                  <span>{formatTimelineTime(event.ts)}</span>
                  {event.actor !== "system" && (
                    <>
                      <span className="text-muted-foreground/50">by</span>
                      <span className="capitalize">{event.actor}</span>
                    </>
                  )}
                </div>
              </div>
            </div>
          ))}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}

// ---------------------------------------------------------------------------
// Properties sidebar
// ---------------------------------------------------------------------------

function PropertyRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-2 group">
      <span className="text-sm text-muted-foreground shrink-0">{label}</span>
      <div className="text-sm text-right min-w-0 flex items-center justify-end">{children}</div>
    </div>
  );
}

function PropertiesSidebar({ task, projectName }: { task: Task; projectName: string }) {
  return (
    <div className="space-y-1">
      <PropertyRow label="Status">
        <StateBadge state={task.state} />
      </PropertyRow>

      <PropertyRow label="Priority">
        <span className="text-sm">
          {task.priority === 1
            ? "High"
            : task.priority === 2
              ? "Medium"
              : task.priority === 3
                ? "Low"
                : "None"}
        </span>
      </PropertyRow>

      <PropertyRow label="Project">
        <span className="text-sm">{projectName}</span>
      </PropertyRow>

      <PropertyRow label="Source">
        {sourceDisplay(task)}
      </PropertyRow>

      {task.labels.length > 0 && (
        <PropertyRow label="Labels">
          <div className="flex flex-wrap gap-1 justify-end">
            {task.labels.map((l) => (
              <Badge key={l} variant="outline" className="text-xs">
                {l}
              </Badge>
            ))}
          </div>
        </PropertyRow>
      )}

      <Separator className="my-3" />

      <PropertyRow label="Created">
        <span className="text-xs text-muted-foreground">
          {formatRelativeTime(task.created_at)}
        </span>
      </PropertyRow>

      <PropertyRow label="Updated">
        <span className="text-xs text-muted-foreground">
          {formatRelativeTime(task.updated_at)}
        </span>
      </PropertyRow>

      <PropertyRow label="Retries">
        <span className="text-xs text-muted-foreground">{task.retry_count}</span>
      </PropertyRow>

      {task.session_id && (
        <PropertyRow label="Session">
          <span className="font-mono text-xs text-muted-foreground">
            {task.session_id.slice(0, 12)}
          </span>
        </PropertyRow>
      )}

      {task.parent_id && (
        <PropertyRow label="Parent">
          <Link
            to={`/tasks/${task.parent_id}`}
            className="text-blue-400 hover:underline font-mono text-xs"
          >
            {task.parent_id.slice(0, 8)}
          </Link>
        </PropertyRow>
      )}

      {task.blocked_by.length > 0 && (
        <PropertyRow label="Blocked by">
          <div className="flex flex-wrap gap-1 justify-end">
            {task.blocked_by.map((bid) => (
              <Link
                key={bid}
                to={`/tasks/${bid}`}
                className="text-blue-400 hover:underline font-mono text-xs"
              >
                {bid.slice(0, 8)}
              </Link>
            ))}
          </div>
        </PropertyRow>
      )}

      {/* Failure info */}
      {task.last_failure && (
        <>
          <Separator className="my-3" />
          <div className="text-xs font-medium text-red-400 mb-2">Last Failure</div>
          <FailureInfoSection failure={task.last_failure} />
        </>
      )}

      {/* Event timeline */}
      <Separator className="my-3" />
      <EventTimeline taskId={task.id} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Task Detail Page
// ---------------------------------------------------------------------------

export function TaskDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { snapshot } = useAppState();
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

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  if (!task) {
    return (
      <div className="p-4">
        <Breadcrumb>
          <BreadcrumbList>
            <BreadcrumbItem>
              <BreadcrumbLink asChild>
                <Link to="/">Tasks</Link>
              </BreadcrumbLink>
            </BreadcrumbItem>
            <BreadcrumbSeparator />
            <BreadcrumbItem>
              <BreadcrumbPage>Not found</BreadcrumbPage>
            </BreadcrumbItem>
          </BreadcrumbList>
        </Breadcrumb>
        <p className="text-muted-foreground mt-4">Task not found.</p>
      </div>
    );
  }

  const projectName = projectLabel(task.project, snapshot.projects);

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-3 shrink-0 bg-background">
        <div className="flex items-center gap-2 min-w-0">
          <Breadcrumb>
            <BreadcrumbList>
              <BreadcrumbItem>
                <BreadcrumbLink asChild>
                  <Link to="/" className="text-muted-foreground hover:text-foreground">
                    Tasks
                  </Link>
                </BreadcrumbLink>
              </BreadcrumbItem>
              <BreadcrumbSeparator />
              <BreadcrumbItem>
                <BreadcrumbPage className="truncate max-w-lg font-medium">
                  {task.title}
                </BreadcrumbPage>
              </BreadcrumbItem>
            </BreadcrumbList>
          </Breadcrumb>
          <Badge variant="outline" className="font-mono text-xs text-muted-foreground shrink-0">
            {task.id.slice(0, 8)}
          </Badge>
        </div>

        {isSessionActive && (
          <Button
            variant="destructive"
            size="sm"
            onClick={handleCancel}
            disabled={cancelling}
            className="gap-1.5 shrink-0"
          >
            <StopCircle className="h-3.5 w-3.5" />
            {cancelling ? "Cancelling..." : "Cancel"}
          </Button>
        )}
      </div>

      {/* Content: tabbed view + properties sidebar */}
      <div className="flex flex-1 min-h-0">
        {/* Main: tabbed content */}
        <Tabs defaultValue="chat" className="flex-1 flex flex-col min-h-0 min-w-0">
          {/* Tab bar */}
          <div className="flex items-center border-b border-border px-4 shrink-0">
            <TabsList variant="line" className="h-10">
              <TabsTrigger value="chat" className="gap-1.5 px-3">
                <MessageSquare className="h-3.5 w-3.5" />
                Chat
              </TabsTrigger>
              <TabsTrigger value="details" className="gap-1.5 px-3">
                <FileText className="h-3.5 w-3.5" />
                Details
              </TabsTrigger>
            </TabsList>
          </div>

          {/* Chat tab */}
          <TabsContent value="chat" className="flex-1 min-h-0 flex flex-col">
            {id && <SessionView taskId={id} chatEnabled={isSessionActive} />}
          </TabsContent>

          {/* Details tab */}
          <TabsContent value="details" className="flex-1 min-h-0 overflow-auto">
            <div className="p-4">
              {task.description ? (
                <div className="prose prose-sm prose-invert max-w-none">
                  <Markdown remarkPlugins={[remarkGfm]}>{task.description}</Markdown>
                </div>
              ) : (
                <p className="text-muted-foreground text-sm">No description provided.</p>
              )}
            </div>
          </TabsContent>
        </Tabs>

        {/* Properties sidebar */}
        <div className="w-72 shrink-0 border-l border-border bg-muted/20">
          <ScrollArea className="h-full">
            <div className="p-4">
              <div className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-4">
                Properties
              </div>
              <PropertiesSidebar task={task} projectName={projectName} />
            </div>
          </ScrollArea>
        </div>
      </div>
    </div>
  );
}
