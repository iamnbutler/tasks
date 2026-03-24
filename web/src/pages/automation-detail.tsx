import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams, useNavigate } from "react-router-dom";
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
  Calendar,
  Zap,
  PlayCircle,
  Workflow,
  Play,
  Pause,
  Pencil,
  Check,
  AlertCircle,
  Loader2,
} from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useAppState } from "@/hooks/use-app-state";
import {
  fetchAutomationRuns,
  triggerAutomation,
  updateAutomation,
  subscribeEvents,
} from "@/lib/api";
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
import type { Automation, AutomationRun, AutomationState, Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SESSION_PREFIX = "automation-run:";

// ---------------------------------------------------------------------------
// State badge
// ---------------------------------------------------------------------------

const stateConfig: Record<
  AutomationState,
  { label: string; variant: "default" | "secondary" | "outline"; className?: string }
> = {
  active: { label: "Active", variant: "default", className: "bg-green-500/20 text-green-400 border-green-500/30" },
  paused: { label: "Paused", variant: "secondary", className: "bg-yellow-500/20 text-yellow-400 border-yellow-500/30" },
  disabled: { label: "Disabled", variant: "outline", className: "text-muted-foreground" },
};

function StateBadge({ state }: { state: AutomationState }) {
  const config = stateConfig[state];
  return (
    <Badge variant={config.variant} className={cn("text-xs", config.className)}>
      {config.label}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Run status badge
// ---------------------------------------------------------------------------

type RunStatus = AutomationRun["status"];

const runStatusConfig: Record<
  RunStatus,
  { label: string; icon: React.ElementType; className: string }
> = {
  pending: {
    label: "Pending",
    icon: Clock,
    className: "bg-yellow-500/20 text-yellow-400 border-yellow-500/30",
  },
  running: {
    label: "Running",
    icon: Loader2,
    className: "bg-blue-500/20 text-blue-400 border-blue-500/30",
  },
  completed: {
    label: "Completed",
    icon: Check,
    className: "bg-green-500/20 text-green-400 border-green-500/30",
  },
  failed: {
    label: "Failed",
    icon: AlertCircle,
    className: "bg-red-500/20 text-red-400 border-red-500/30",
  },
};

function RunStatusBadge({ status }: { status: RunStatus }) {
  const config = runStatusConfig[status];
  const Icon = config.icon;
  const isRunning = status === "running";

  return (
    <Badge
      variant="outline"
      className={cn("text-xs gap-1 py-0.5", config.className)}
    >
      <Icon className={cn("h-3 w-3", isRunning && "animate-spin")} />
      {config.label}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Trigger type display
// ---------------------------------------------------------------------------

const triggerConfig = {
  schedule: { icon: Calendar, label: "Schedule" },
  event: { icon: Zap, label: "Event" },
  manual: { icon: PlayCircle, label: "Manual" },
};

function TriggerDisplay({ trigger }: { trigger: Automation["trigger"] }) {
  const config = triggerConfig[trigger.type];
  const Icon = config.icon;
  const detail = trigger.type === "schedule" ? trigger.cron : trigger.type === "event" ? trigger.event_type : null;

  return (
    <div className="flex items-center gap-1.5 text-sm">
      <Icon className="h-4 w-4 text-muted-foreground" />
      <span>{config.label}</span>
      {detail && (
        <span className="font-mono text-muted-foreground">({detail})</span>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Parse Claude Code protocol messages (shared with task-detail)
// ---------------------------------------------------------------------------

interface ParsedBlock {
  kind: "text" | "thinking" | "tool_use" | "tool_result" | "error" | "system" | "lifecycle" | "human_message" | "session_boundary";
  content: string;
  toolName?: string;
  filePath?: string;
  timestamp?: string;
}

const lifecycleLabels: Record<string, string> = {
  "task:created": "Run created",
  "task:state:running": "Agent started",
  "task:state:question": "Agent has a question",
  "task:state:waiting": "Run waiting",
  "task:state:blocked": "Run blocked",
  "task:state:testing": "Running tests",
  "task:state:awaiting_merge": "Awaiting merge",
  "task:state:conflict": "Merge conflict detected",
  "task:state:completed": "Run completed",
  "task:state:failed": "Run failed",
  "task:state:cancelled": "Run cancelled",
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

    if (event.type === "human:message") {
      const message = event.data?.message as string | undefined;
      if (message) {
        blocks.push({ kind: "human_message", content: message, timestamp: event.ts });
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
// Fetch events for automation run
// ---------------------------------------------------------------------------

async function fetchRunEvents(runId: string): Promise<Event[]> {
  const sessionId = `${SESSION_PREFIX}${runId}`;
  const res = await fetch(`/api/tasks/${sessionId}/events`);
  if (!res.ok) {
    // No events yet or session not found
    return [];
  }
  return res.json();
}

// ---------------------------------------------------------------------------
// Send chat to automation run
// ---------------------------------------------------------------------------

async function sendRunChat(runId: string, message: string): Promise<void> {
  const sessionId = `${SESSION_PREFIX}${runId}`;
  const res = await fetch(`/api/tasks/${sessionId}/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message }),
  });
  if (!res.ok) {
    throw new Error(`Failed to send chat: ${res.status}`);
  }
}

// ---------------------------------------------------------------------------
// Session view for automation run
// ---------------------------------------------------------------------------

function RunSessionView({ runId, chatEnabled }: { runId: string; chatEnabled: boolean }) {
  const [rawEvents, setRawEvents] = useState<Event[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [sending, setSending] = useState(false);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  const sessionId = `${SESSION_PREFIX}${runId}`;

  useEffect(() => {
    const isRelevant = (e: Event) =>
      e.type === "agent:message" ||
      e.type === "agent:question" ||
      e.type === "agent:error" ||
      e.type === "human:message" ||
      e.type.startsWith("task:");

    fetchRunEvents(runId).then((events) => {
      setRawEvents(events.filter(isRelevant).sort((a, b) => a.ts.localeCompare(b.ts)));
    });

    const source = subscribeEvents({ task_id: sessionId });
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
  }, [runId, sessionId]);

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
      await sendRunChat(runId, text);
    } catch {
      // Message will show via SSE if it worked
    } finally {
      setSending(false);
    }
  }, [chatInput, sending, runId]);

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
// Duration formatting
// ---------------------------------------------------------------------------

function formatDuration(startedAt: string, completedAt?: string): string {
  const start = new Date(startedAt).getTime();
  const end = completedAt ? new Date(completedAt).getTime() : Date.now();
  const diffMs = end - start;

  if (diffMs < 1000) return "<1s";
  if (diffMs < 60000) return `${Math.floor(diffMs / 1000)}s`;
  if (diffMs < 3600000) {
    const mins = Math.floor(diffMs / 60000);
    const secs = Math.floor((diffMs % 60000) / 1000);
    return secs > 0 ? `${mins}m ${secs}s` : `${mins}m`;
  }
  const hours = Math.floor(diffMs / 3600000);
  const mins = Math.floor((diffMs % 3600000) / 60000);
  return mins > 0 ? `${hours}h ${mins}m` : `${hours}h`;
}

// ---------------------------------------------------------------------------
// Runs list sidebar
// ---------------------------------------------------------------------------

function RunsList({
  runs,
  selectedRunId,
  onSelectRun,
  loading,
}: {
  runs: AutomationRun[];
  selectedRunId: string | null;
  onSelectRun: (run: AutomationRun) => void;
  loading: boolean;
}) {
  if (loading) {
    return (
      <div className="flex items-center justify-center py-8">
        <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (runs.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-8 px-4 text-center">
        <Clock className="h-6 w-6 text-muted-foreground mb-2" />
        <p className="text-sm text-muted-foreground">No runs yet</p>
        <p className="text-xs text-muted-foreground/70 mt-1">
          Trigger this automation to see run history
        </p>
      </div>
    );
  }

  return (
    <div className="divide-y divide-border">
      {runs.map((run) => (
        <button
          key={run.id}
          onClick={() => onSelectRun(run)}
          className={cn(
            "flex items-center gap-3 w-full px-3 py-2.5 text-left transition-colors",
            selectedRunId === run.id
              ? "bg-accent"
              : "hover:bg-accent/50"
          )}
        >
          <RunStatusBadge status={run.status} />
          <span
            className="text-xs text-muted-foreground flex-1"
            title={new Date(run.started_at).toLocaleString()}
          >
            {formatRelativeTime(run.started_at)}
          </span>
          {(run.status === "completed" || run.status === "failed") && (
            <span className="text-xs text-muted-foreground/70">
              {formatDuration(run.started_at, run.completed_at)}
            </span>
          )}
          <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
        </button>
      ))}
    </div>
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

function PropertiesSidebar({ automation, projectName }: { automation: Automation; projectName: string }) {
  return (
    <div className="space-y-1">
      <PropertyRow label="Status">
        <StateBadge state={automation.state} />
      </PropertyRow>

      <PropertyRow label="Trigger">
        <TriggerDisplay trigger={automation.trigger} />
      </PropertyRow>

      <PropertyRow label="Project">
        <span className="text-sm">{projectName}</span>
      </PropertyRow>

      <Separator className="my-3" />

      <PropertyRow label="Created">
        <span className="text-xs text-muted-foreground">
          {formatRelativeTime(automation.created_at)}
        </span>
      </PropertyRow>

      <PropertyRow label="Updated">
        <span className="text-xs text-muted-foreground">
          {formatRelativeTime(automation.updated_at)}
        </span>
      </PropertyRow>

      {automation.compiled_workflow && (
        <>
          <Separator className="my-3" />
          <Collapsible>
            <CollapsibleTrigger className="flex items-center gap-2 w-full py-2 text-left hover:bg-muted/50 rounded-md px-2 -mx-2 transition-colors">
              <ChevronRight className="h-4 w-4 text-muted-foreground transition-transform data-[state=open]:rotate-90" />
              <span className="text-sm font-medium">Compiled Workflow</span>
            </CollapsibleTrigger>
            <CollapsibleContent>
              <div className="mt-2 rounded-md border border-border bg-muted/50 p-2">
                <pre className="text-xs font-mono whitespace-pre-wrap break-words text-muted-foreground max-h-40 overflow-y-auto">
                  {automation.compiled_workflow}
                </pre>
              </div>
            </CollapsibleContent>
          </Collapsible>
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Automation Detail Page
// ---------------------------------------------------------------------------

export function AutomationDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { automations, snapshot, refreshAutomations } = useAppState();
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [runsLoading, setRunsLoading] = useState(true);
  const [selectedRun, setSelectedRun] = useState<AutomationRun | null>(null);
  const [triggering, setTriggering] = useState(false);

  const automation = automations.find((a) => a.id === id);

  // Load runs
  const loadRuns = useCallback(async () => {
    if (!id) return;
    try {
      const data = await fetchAutomationRuns(id);
      const sorted = [...data].sort(
        (a, b) =>
          new Date(b.started_at).getTime() - new Date(a.started_at).getTime()
      );
      setRuns(sorted);

      // If a run is selected, update it with the latest data
      if (selectedRun) {
        const updated = sorted.find((r) => r.id === selectedRun.id);
        if (updated) {
          setSelectedRun(updated);
        }
      }

      // Auto-select the first running run if none selected
      if (!selectedRun) {
        const runningRun = sorted.find((r) => r.status === "running");
        if (runningRun) {
          setSelectedRun(runningRun);
        }
      }
    } catch (err) {
      console.error("Failed to load runs:", err);
    } finally {
      setRunsLoading(false);
    }
  }, [id, selectedRun]);

  // Initial load + polling
  useEffect(() => {
    loadRuns();
    const hasRunningRun = runs.some((r) => r.status === "running" || r.status === "pending");
    const interval = setInterval(loadRuns, hasRunningRun ? 2000 : 5000);
    return () => clearInterval(interval);
  }, [runs, loadRuns]);

  const handleTrigger = useCallback(async () => {
    if (!id || triggering) return;
    setTriggering(true);
    try {
      const newRun = await triggerAutomation(id);
      await loadRuns();
      setSelectedRun(newRun);
    } catch (err) {
      console.error("Failed to trigger automation:", err);
    } finally {
      setTriggering(false);
    }
  }, [id, triggering, loadRuns]);

  const handleToggleState = useCallback(async () => {
    if (!automation) return;
    try {
      const newState: AutomationState = automation.state === "active" ? "paused" : "active";
      await updateAutomation(automation.id, { state: newState });
      await refreshAutomations();
    } catch (err) {
      console.error("Failed to toggle automation state:", err);
    }
  }, [automation, refreshAutomations]);

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  if (!automation) {
    return (
      <div className="p-4">
        <Breadcrumb>
          <BreadcrumbList>
            <BreadcrumbItem>
              <BreadcrumbLink asChild>
                <Link to="/automations">Automations</Link>
              </BreadcrumbLink>
            </BreadcrumbItem>
            <BreadcrumbSeparator />
            <BreadcrumbItem>
              <BreadcrumbPage>Not found</BreadcrumbPage>
            </BreadcrumbItem>
          </BreadcrumbList>
        </Breadcrumb>
        <p className="text-muted-foreground mt-4">Automation not found.</p>
      </div>
    );
  }

  const projectName = projectLabel(automation.project_id, snapshot.projects);
  const isRunActive = selectedRun?.status === "running";

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-3 shrink-0 bg-background">
        <div className="flex items-center gap-2 min-w-0">
          <Breadcrumb>
            <BreadcrumbList>
              <BreadcrumbItem>
                <BreadcrumbLink asChild>
                  <Link to="/automations" className="text-muted-foreground hover:text-foreground">
                    Automations
                  </Link>
                </BreadcrumbLink>
              </BreadcrumbItem>
              <BreadcrumbSeparator />
              <BreadcrumbItem>
                <BreadcrumbPage className="truncate max-w-lg font-medium">
                  {automation.name}
                </BreadcrumbPage>
              </BreadcrumbItem>
            </BreadcrumbList>
          </Breadcrumb>
          <StateBadge state={automation.state} />
        </div>

        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={handleToggleState}
            className="gap-1.5"
          >
            {automation.state === "active" ? (
              <>
                <Pause className="h-3.5 w-3.5" />
                Pause
              </>
            ) : (
              <>
                <Play className="h-3.5 w-3.5" />
                Resume
              </>
            )}
          </Button>
          <Button
            size="sm"
            onClick={handleTrigger}
            disabled={triggering || automation.state === "disabled"}
            className="gap-1.5"
          >
            <Play className={cn("h-3.5 w-3.5", triggering && "animate-pulse")} />
            {triggering ? "Starting..." : "Run Now"}
          </Button>
        </div>
      </div>

      {/* Content */}
      <div className="flex flex-1 min-h-0">
        {/* Left: Runs list */}
        <div className="w-64 shrink-0 border-r border-border bg-muted/10 flex flex-col">
          <div className="px-3 py-2 border-b border-border">
            <div className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">
              Runs ({runs.length})
            </div>
          </div>
          <ScrollArea className="flex-1">
            <RunsList
              runs={runs}
              selectedRunId={selectedRun?.id ?? null}
              onSelectRun={setSelectedRun}
              loading={runsLoading}
            />
          </ScrollArea>
        </div>

        {/* Center: Main content */}
        <div className="flex-1 flex flex-col min-h-0 min-w-0">
          {selectedRun ? (
            <Tabs defaultValue="chat" className="flex-1 flex flex-col min-h-0">
              {/* Tab bar */}
              <div className="flex items-center justify-between border-b border-border px-4 shrink-0">
                <TabsList variant="line" className="h-10">
                  <TabsTrigger value="chat" className="gap-1.5 px-3">
                    <MessageSquare className="h-3.5 w-3.5" />
                    Chat
                  </TabsTrigger>
                  <TabsTrigger value="details" className="gap-1.5 px-3">
                    <FileText className="h-3.5 w-3.5" />
                    Prompt
                  </TabsTrigger>
                </TabsList>
                <div className="flex items-center gap-2">
                  <RunStatusBadge status={selectedRun.status} />
                  <span className="text-xs text-muted-foreground">
                    {formatDuration(selectedRun.started_at, selectedRun.completed_at)}
                  </span>
                </div>
              </div>

              {/* Chat tab */}
              <TabsContent value="chat" className="flex-1 min-h-0 flex flex-col">
                <RunSessionView runId={selectedRun.id} chatEnabled={isRunActive} />
              </TabsContent>

              {/* Details tab */}
              <TabsContent value="details" className="flex-1 min-h-0 overflow-auto">
                <div className="p-4">
                  {automation.prompt ? (
                    <div className="prose prose-sm prose-invert max-w-none">
                      <Markdown remarkPlugins={[remarkGfm]}>{automation.prompt}</Markdown>
                    </div>
                  ) : (
                    <p className="text-muted-foreground text-sm">No prompt provided.</p>
                  )}

                  {selectedRun.error && (
                    <div className="mt-4">
                      <div className="text-xs font-medium text-red-400 mb-2">Error</div>
                      <div className="rounded-md border border-red-500/30 bg-red-500/10 p-2">
                        <pre className="text-xs text-red-300 whitespace-pre-wrap break-words font-mono">
                          {selectedRun.error}
                        </pre>
                      </div>
                    </div>
                  )}

                  {selectedRun.output && (
                    <div className="mt-4">
                      <div className="text-xs font-medium text-muted-foreground mb-2">Output</div>
                      <div className="rounded-md border border-border bg-muted/50 p-2">
                        <pre className="text-xs whitespace-pre-wrap break-words font-mono">
                          {selectedRun.output}
                        </pre>
                      </div>
                    </div>
                  )}
                </div>
              </TabsContent>
            </Tabs>
          ) : (
            <div className="flex-1 flex items-center justify-center">
              <div className="text-center">
                <Workflow className="h-12 w-12 text-muted-foreground/50 mx-auto mb-4" />
                <p className="text-muted-foreground">
                  {runs.length === 0
                    ? "No runs yet. Click \"Run Now\" to start."
                    : "Select a run to view details"}
                </p>
              </div>
            </div>
          )}
        </div>

        {/* Right: Properties sidebar */}
        <div className="w-72 shrink-0 border-l border-border bg-muted/20">
          <ScrollArea className="h-full">
            <div className="p-4">
              <div className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-4">
                Properties
              </div>
              <PropertiesSidebar automation={automation} projectName={projectName} />
            </div>
          </ScrollArea>
        </div>
      </div>
    </div>
  );
}
