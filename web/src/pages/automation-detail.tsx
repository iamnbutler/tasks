import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ChevronDown,
  ChevronRight,
  FileText,
  Terminal,
  Bot,
  Clock,
  Check,
  AlertCircle,
  Loader2,
  Calendar,
  Zap,
  PlayCircle,
  Workflow,
  Play,
  RotateCcw,
  XCircle,
} from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useAppState } from "@/hooks/use-app-state";
import {
  fetchAutomationRuns,
  fetchAutomationRunEvents,
  triggerAutomation,
  subscribeEvents,
} from "@/lib/api";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import type { Automation, AutomationRun, AutomationState, Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// State badge configuration
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
  cancelled: {
    label: "Cancelled",
    icon: XCircle,
    className: "bg-gray-500/20 text-gray-400 border-gray-500/30",
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
      <Icon
        className={cn("h-3 w-3", isRunning && "animate-spin")}
      />
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
    <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
      <Icon className="h-3.5 w-3.5" />
      <span>{config.label}</span>
      {detail && (
        <span className="font-mono text-muted-foreground/70">({detail})</span>
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
// Parse agent events (same as task-detail)
// ---------------------------------------------------------------------------

interface ParsedBlock {
  kind: "text" | "tool_use" | "tool_result" | "error" | "lifecycle" | "session_boundary" | "output_chunk";
  content: string;
  toolName?: string;
  filePath?: string;
  timestamp?: string;
}

const lifecycleLabels: Record<string, string> = {
  "automation:run:started": "Run started",
  "automation:run:completed": "Run completed",
  "automation:run:failed": "Run failed",
};

function parseAgentEvents(events: Event[]): ParsedBlock[] {
  const blocks: ParsedBlock[] = [];

  for (const event of events) {
    // Handle automation lifecycle events
    if (event.type.startsWith("automation:run:")) {
      if (event.type === "automation:run:output") {
        // Streaming output chunk
        const chunk = event.data?.chunk as string | undefined;
        if (chunk) {
          blocks.push({ kind: "output_chunk", content: chunk, timestamp: event.ts });
        }
        continue;
      }
      const label = lifecycleLabels[event.type];
      if (label) {
        blocks.push({ kind: "lifecycle", content: label, timestamp: event.ts });
      }
      continue;
    }

    // Handle agent errors
    if (event.type === "agent:error") {
      blocks.push({
        kind: "error",
        content: typeof event.data?.text === "string" ? event.data.text : JSON.stringify(event.data),
        timestamp: event.ts,
      });
      continue;
    }

    // Handle agent messages (same parsing as task-detail)
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

  if (block.kind === "output_chunk") {
    return (
      <div className="font-mono text-sm text-muted-foreground whitespace-pre-wrap">
        {block.content}
      </div>
    );
  }

  // Text block (agent message)
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
// Run session view (shows events for a specific run)
// ---------------------------------------------------------------------------

function RunSessionView({ automationId, run }: { automationId: string; run: AutomationRun }) {
  const [rawEvents, setRawEvents] = useState<Event[]>([]);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const isRelevant = (e: Event) =>
      e.type === "agent:message" ||
      e.type === "agent:error" ||
      e.type.startsWith("automation:run:");

    // Load historical events
    fetchAutomationRunEvents(automationId, run.id).then((events) => {
      setRawEvents(events.filter(isRelevant).sort((a, b) => a.ts.localeCompare(b.ts)));
    });

    // Subscribe to live events for this run
    const sessionId = `automation-run:${run.id}`;
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
  }, [automationId, run.id]);

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

  // If the run only has output (not agent messages), show a simple output view
  const hasAgentMessages = blocks.some(b => b.kind === "text" || b.kind === "tool_use" || b.kind === "tool_result");
  const hasOutputChunks = blocks.some(b => b.kind === "output_chunk");

  if (!hasAgentMessages && (run.output || hasOutputChunks)) {
    // Simple output view for direct LLM execution
    const outputContent = hasOutputChunks
      ? blocks.filter(b => b.kind === "output_chunk").map(b => b.content).join("")
      : run.output;

    return (
      <div className="flex flex-col flex-1 min-h-0">
        <div className="relative flex-1 min-h-0">
          <ScrollArea className="absolute inset-0">
            <div className="p-4 space-y-2">
              {/* Run status header */}
              <div className="flex items-center gap-2 pb-2 border-b border-border mb-4">
                <RunStatusBadge status={run.status} />
                <span className="text-xs text-muted-foreground">
                  Started {formatRelativeTime(run.started_at)}
                </span>
                {run.completed_at && (
                  <span className="text-xs text-muted-foreground">
                    - Duration: {formatDuration(run.started_at, run.completed_at)}
                  </span>
                )}
              </div>

              {/* Error message */}
              {run.error && (
                <div className="rounded-md border border-red-500/30 bg-red-500/10 p-3">
                  <div className="text-xs font-medium text-red-400 mb-1">Error</div>
                  <pre className="text-xs text-red-300 whitespace-pre-wrap break-words font-mono">
                    {run.error}
                  </pre>
                </div>
              )}

              {/* Output */}
              {outputContent && (
                <div className="prose prose-sm prose-invert max-w-none">
                  <Markdown remarkPlugins={[remarkGfm]}>{outputContent}</Markdown>
                </div>
              )}

              {/* No output */}
              {!outputContent && !run.error && run.status === "completed" && (
                <p className="text-muted-foreground text-sm italic">No output recorded</p>
              )}

              {/* Pending/running without output yet */}
              {!outputContent && !run.error && (run.status === "pending" || run.status === "running") && (
                <div className="flex items-center gap-2 text-muted-foreground text-sm">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  <span>Waiting for output...</span>
                </div>
              )}
            </div>
          </ScrollArea>
        </div>
      </div>
    );
  }

  // Chat/session view for container-based execution
  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Message stream */}
      <div className="relative flex-1 min-h-0">
        <div
          ref={scrollContainerRef}
          onScroll={handleScroll}
          className="absolute inset-0 overflow-y-auto p-4 space-y-2"
        >
          {/* Run status header */}
          <div className="flex items-center gap-2 pb-2 border-b border-border mb-2">
            <RunStatusBadge status={run.status} />
            <span className="text-xs text-muted-foreground">
              Started {formatRelativeTime(run.started_at)}
            </span>
            {run.completed_at && (
              <span className="text-xs text-muted-foreground">
                - Duration: {formatDuration(run.started_at, run.completed_at)}
              </span>
            )}
          </div>

          {blocks.length === 0 && run.status === "running" && (
            <div className="flex items-center gap-2 text-muted-foreground text-sm py-8 justify-center">
              <Loader2 className="h-4 w-4 animate-spin" />
              <span>Agent is working...</span>
            </div>
          )}
          {blocks.length === 0 && run.status !== "running" && (
            <p className="text-muted-foreground text-center py-8 text-sm">
              No agent output recorded.
            </p>
          )}
          {blocks.map((block, i) => (
            <BlockView key={i} block={block} />
          ))}

          {/* Show error at the end if present */}
          {run.error && run.status === "failed" && (
            <div className="rounded-md border border-red-500/30 bg-red-500/10 p-3 mt-4">
              <div className="text-xs font-medium text-red-400 mb-1">Run Failed</div>
              <pre className="text-xs text-red-300 whitespace-pre-wrap break-words font-mono">
                {run.error}
              </pre>
            </div>
          )}

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
    </div>
  );
}

// ---------------------------------------------------------------------------
// Run list item
// ---------------------------------------------------------------------------

function RunListItem({
  run,
  isSelected,
  onSelect,
}: {
  run: AutomationRun;
  isSelected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      onClick={onSelect}
      className={cn(
        "flex items-center gap-3 w-full px-3 py-2 text-left transition-colors",
        isSelected ? "bg-accent" : "hover:bg-accent/50"
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
    </button>
  );
}

// ---------------------------------------------------------------------------
// Automation Detail Page
// ---------------------------------------------------------------------------

export function AutomationDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { snapshot, automations, error, refreshSnapshot } = useAppState();
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  const [loadingRuns, setLoadingRuns] = useState(true);
  const [triggering, setTriggering] = useState(false);

  const automation = automations.find((a) => a.id === id);

  // Load runs
  const loadRuns = useCallback(async () => {
    if (!id) return;
    try {
      const data = await fetchAutomationRuns(id);
      const sorted = [...data].sort(
        (a, b) => new Date(b.started_at).getTime() - new Date(a.started_at).getTime()
      );
      setRuns(sorted);
      // Auto-select the first run if none selected
      const firstRun = sorted[0];
      if (!selectedRunId && firstRun) {
        setSelectedRunId(firstRun.id);
      }
    } catch (error) {
      console.error("Failed to load runs:", error);
    } finally {
      setLoadingRuns(false);
    }
  }, [id, selectedRunId]);

  useEffect(() => {
    loadRuns();
  }, [loadRuns]);

  // Poll for updates when there's a running run
  const runsRef = useRef<AutomationRun[]>([]);
  runsRef.current = runs;

  useEffect(() => {
    const hasRunningRun = runs.some((r) => r.status === "running" || r.status === "pending");
    const intervalMs = hasRunningRun ? 2000 : 5000;

    const interval = setInterval(() => {
      loadRuns();
    }, intervalMs);

    return () => clearInterval(interval);
  }, [runs, loadRuns]);

  const handleTrigger = async () => {
    if (!id || triggering) return;
    setTriggering(true);
    try {
      const newRun = await triggerAutomation(id);
      await loadRuns();
      setSelectedRunId(newRun.id);
    } catch (error) {
      console.error("Failed to trigger automation:", error);
    } finally {
      setTriggering(false);
    }
  };

  const selectedRun = runs.find((r) => r.id === selectedRunId);

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        {error ? (
          <div className="flex flex-col items-center gap-3">
            <p className="text-destructive text-sm">Failed to load data: {error.message}</p>
            <button
              onClick={() => refreshSnapshot()}
              className="text-sm text-muted-foreground hover:text-foreground underline"
            >
              Retry
            </button>
          </div>
        ) : (
          <p className="text-muted-foreground text-sm">Loading...</p>
        )}
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

  const projectName = snapshot.projects.find((p) => p.id === automation.project_id)?.repo ?? automation.project_id;

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-3 shrink-0 bg-background">
        <div className="flex items-center gap-3 min-w-0">
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
                <BreadcrumbPage className="truncate max-w-lg font-medium flex items-center gap-2">
                  <Workflow className="h-4 w-4" />
                  {automation.name}
                </BreadcrumbPage>
              </BreadcrumbItem>
            </BreadcrumbList>
          </Breadcrumb>
          <StateBadge state={automation.state} />
        </div>

        <Button
          size="sm"
          onClick={handleTrigger}
          disabled={triggering || automation.state === "disabled"}
          className="gap-1.5 shrink-0"
        >
          <Play className={cn("h-3.5 w-3.5", triggering && "animate-pulse")} />
          {triggering ? "Running..." : "Run Now"}
        </Button>
      </div>

      {/* Content: run list + session view */}
      <div className="flex flex-1 min-h-0">
        {/* Left: Run list sidebar */}
        <div className="w-64 shrink-0 border-r border-border bg-muted/20 flex flex-col">
          <div className="px-3 py-2 border-b border-border">
            <div className="flex items-center justify-between">
              <span className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">
                Runs
              </span>
              <span className="text-xs text-muted-foreground">
                {runs.length}
              </span>
            </div>
          </div>
          <ScrollArea className="flex-1">
            {loadingRuns ? (
              <div className="flex items-center justify-center py-8">
                <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
              </div>
            ) : runs.length === 0 ? (
              <div className="flex flex-col items-center justify-center py-8 px-4 text-center">
                <Clock className="h-6 w-6 text-muted-foreground mb-2" />
                <p className="text-sm text-muted-foreground">No runs yet</p>
                <p className="text-xs text-muted-foreground/70 mt-1">
                  Click "Run Now" to trigger this automation
                </p>
              </div>
            ) : (
              <div className="divide-y divide-border">
                {runs.map((run) => (
                  <RunListItem
                    key={run.id}
                    run={run}
                    isSelected={run.id === selectedRunId}
                    onSelect={() => setSelectedRunId(run.id)}
                  />
                ))}
              </div>
            )}
          </ScrollArea>
        </div>

        {/* Right: Session/output view */}
        <div className="flex-1 flex flex-col min-h-0 min-w-0">
          {selectedRun ? (
            <RunSessionView automationId={automation.id} run={selectedRun} />
          ) : (
            <div className="flex flex-col items-center justify-center h-full text-center p-8">
              <Workflow className="h-10 w-10 text-muted-foreground mb-4" />
              <p className="text-muted-foreground text-sm">
                {runs.length === 0 ? "Run this automation to see output" : "Select a run to view details"}
              </p>
            </div>
          )}
        </div>

        {/* Properties sidebar */}
        <div className="w-64 shrink-0 border-l border-border bg-muted/20">
          <ScrollArea className="h-full">
            <div className="p-4">
              <div className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-4">
                Properties
              </div>
              <div className="space-y-3">
                <div className="flex items-center justify-between gap-4 py-1">
                  <span className="text-sm text-muted-foreground">Project</span>
                  <span className="text-sm truncate">{projectName}</span>
                </div>
                <div className="flex items-center justify-between gap-4 py-1">
                  <span className="text-sm text-muted-foreground">Trigger</span>
                  <TriggerDisplay trigger={automation.trigger} />
                </div>
                <div className="flex items-center justify-between gap-4 py-1">
                  <span className="text-sm text-muted-foreground">State</span>
                  <StateBadge state={automation.state} />
                </div>
                <div className="flex items-center justify-between gap-4 py-1">
                  <span className="text-sm text-muted-foreground">Created</span>
                  <span className="text-xs text-muted-foreground">
                    {formatRelativeTime(automation.created_at)}
                  </span>
                </div>
                <div className="flex items-center justify-between gap-4 py-1">
                  <span className="text-sm text-muted-foreground">Updated</span>
                  <span className="text-xs text-muted-foreground">
                    {formatRelativeTime(automation.updated_at)}
                  </span>
                </div>
                {automation.prompt && (
                  <div className="pt-3 border-t border-border">
                    <span className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">
                      Prompt
                    </span>
                    <p className="text-sm text-muted-foreground mt-2 whitespace-pre-wrap">
                      {automation.prompt}
                    </p>
                  </div>
                )}
              </div>
            </div>
          </ScrollArea>
        </div>
      </div>
    </div>
  );
}
