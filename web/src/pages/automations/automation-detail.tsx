import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  Calendar,
  ChevronDown,
  ChevronRight,
  Clock,
  ExternalLink,
  FileText,
  Loader2,
  MessageSquare,
  Pause,
  Play,
  PlayCircle,
  Send,
  StopCircle,
  Terminal,
  Workflow,
  Zap,
  AlertCircle,
  Check,
  Bot,
} from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useAppState } from "@/hooks/use-app-state";
import {
  fetchAutomation,
  fetchAutomationRuns,
  fetchRunEvents,
  triggerAutomation,
  updateAutomation,
  subscribeEvents,
} from "@/lib/api";
import { cn, formatRelativeTime, projectLabel } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
// Run status badge configuration
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
// Parse run output events
// ---------------------------------------------------------------------------

interface ParsedBlock {
  kind: "text" | "tool_use" | "tool_result" | "error" | "lifecycle" | "output_chunk";
  content: string;
  toolName?: string;
  timestamp?: string;
}

function parseRunEvents(events: Event[]): ParsedBlock[] {
  const blocks: ParsedBlock[] = [];

  for (const event of events) {
    if (event.type === "automation:run:started") {
      blocks.push({ kind: "lifecycle", content: "Run started", timestamp: event.ts });
      continue;
    }

    if (event.type === "automation:run:output") {
      const chunk = event.data?.chunk as string | undefined;
      if (chunk) {
        blocks.push({ kind: "output_chunk", content: chunk, timestamp: event.ts });
      }
      continue;
    }

    if (event.type === "automation:run:completed") {
      blocks.push({ kind: "lifecycle", content: "Run completed", timestamp: event.ts });
      continue;
    }

    if (event.type === "automation:run:failed") {
      const error = event.data?.error as string | undefined;
      blocks.push({
        kind: "error",
        content: error || "Run failed",
        timestamp: event.ts
      });
      continue;
    }

    // Handle agent messages if the run uses a container session
    if (event.type === "agent:message" || event.type === "agent:question") {
      const raw = event.data?.text;
      if (typeof raw !== "string") continue;

      let msg: Record<string, unknown>;
      try {
        msg = JSON.parse(raw);
      } catch {
        if (raw.trim()) blocks.push({ kind: "text", content: raw, timestamp: event.ts });
        continue;
      }

      if (msg.type === "system") continue;

      if (msg.type === "result") {
        const result = msg.result as Record<string, unknown> | undefined;
        if (typeof result?.text === "string" && result.text) {
          blocks.push({ kind: "text", content: result.text, timestamp: event.ts });
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

    if (event.type === "agent:error") {
      blocks.push({
        kind: "error",
        content: typeof event.data?.text === "string" ? event.data.text : JSON.stringify(event.data),
        timestamp: event.ts,
      });
      continue;
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

  // Text block (agent response)
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
// Run output view (real-time streaming)
// ---------------------------------------------------------------------------

function RunOutputView({ run }: { run: AutomationRun }) {
  const [rawEvents, setRawEvents] = useState<Event[]>([]);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const isRelevant = (e: Event) =>
      e.type === "automation:run:started" ||
      e.type === "automation:run:output" ||
      e.type === "automation:run:completed" ||
      e.type === "automation:run:failed" ||
      e.type === "agent:message" ||
      e.type === "agent:question" ||
      e.type === "agent:error";

    // Fetch historical events
    fetchRunEvents(run.id).then((events) => {
      setRawEvents(events.filter(isRelevant).sort((a, b) => a.ts.localeCompare(b.ts)));
    }).catch(() => {
      // Ignore errors - the run might not have events yet
    });

    // Subscribe to live events for this run
    const source = subscribeEvents({ task_id: run.id });
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
  }, [run.id]);

  const blocks = parseRunEvents(rawEvents);
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

  // If no live events but run has final output, show that
  const showFinalOutput = blocks.length === 0 && (run.output || run.error);

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="relative flex-1 min-h-0">
        <div
          ref={scrollContainerRef}
          onScroll={handleScroll}
          className="absolute inset-0 overflow-y-auto p-4 space-y-2"
        >
          {blocks.length === 0 && !showFinalOutput && (
            <p className="text-muted-foreground text-center py-8 text-sm">
              {run.status === "pending" ? "Waiting to start..." :
               run.status === "running" ? "Waiting for output..." : "No output recorded."}
            </p>
          )}
          {blocks.map((block, i) => (
            <BlockView key={i} block={block} />
          ))}
          {showFinalOutput && run.output && (
            <div className="space-y-2">
              <div className="flex items-center gap-2 py-1.5 text-sm text-muted-foreground">
                <div className="h-px flex-1 bg-border" />
                <span>Output</span>
                <div className="h-px flex-1 bg-border" />
              </div>
              <pre className="font-mono text-sm whitespace-pre-wrap text-muted-foreground bg-muted/50 rounded-md p-3">
                {run.output}
              </pre>
            </div>
          )}
          {showFinalOutput && run.error && (
            <div className="rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-400">
              {run.error}
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
            New output
          </button>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Run history list
// ---------------------------------------------------------------------------

function RunRow({
  run,
  isSelected,
  onSelect
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
      <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
    </button>
  );
}

function RunHistoryList({
  runs,
  selectedRunId,
  onSelectRun,
}: {
  runs: AutomationRun[];
  selectedRunId: string | null;
  onSelectRun: (run: AutomationRun) => void;
}) {
  if (runs.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center py-12 px-4 text-center">
        <Clock className="h-8 w-8 text-muted-foreground mb-2" />
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
        <RunRow
          key={run.id}
          run={run}
          isSelected={selectedRunId === run.id}
          onSelect={() => onSelectRun(run)}
        />
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
      <PropertyRow label="State">
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
    </div>
  );
}

// ---------------------------------------------------------------------------
// Automation Detail Page
// ---------------------------------------------------------------------------

export function AutomationDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { snapshot, refreshAutomations } = useAppState();
  const [automation, setAutomation] = useState<Automation | null>(null);
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [selectedRun, setSelectedRun] = useState<AutomationRun | null>(null);
  const [loading, setLoading] = useState(true);
  const [triggering, setTriggering] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const projects = snapshot?.projects ?? [];
  const projectName = automation
    ? projectLabel(automation.project_id, projects)
    : "";

  // Load automation and runs
  useEffect(() => {
    if (!id) return;

    async function load() {
      setLoading(true);
      setError(null);
      try {
        const [auto, runsList] = await Promise.all([
          fetchAutomation(id!),
          fetchAutomationRuns(id!),
        ]);
        setAutomation(auto);
        // Sort runs by started_at descending (most recent first)
        const sorted = [...runsList].sort(
          (a, b) => new Date(b.started_at).getTime() - new Date(a.started_at).getTime()
        );
        setRuns(sorted);
        // Auto-select the most recent running run, or the first run
        const runningRun = sorted.find((r) => r.status === "running" || r.status === "pending");
        if (runningRun) {
          setSelectedRun(runningRun);
        } else if (sorted.length > 0 && !selectedRun) {
          const firstRun = sorted[0];
          if (firstRun) {
            setSelectedRun(firstRun);
          }
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : "Failed to load automation");
      } finally {
        setLoading(false);
      }
    }

    load();
  }, [id]);

  // Poll for run updates
  useEffect(() => {
    if (!id) return;

    const hasActiveRun = runs.some((r) => r.status === "running" || r.status === "pending");
    const interval = hasActiveRun ? 2000 : 5000;

    const timer = setInterval(async () => {
      try {
        const runsList = await fetchAutomationRuns(id);
        const sorted = [...runsList].sort(
          (a, b) => new Date(b.started_at).getTime() - new Date(a.started_at).getTime()
        );
        setRuns(sorted);

        // Update selected run if it changed
        if (selectedRun) {
          const updated = sorted.find((r) => r.id === selectedRun.id);
          if (updated) {
            setSelectedRun(updated);
          }
        }
      } catch {
        // Ignore polling errors
      }
    }, interval);

    return () => clearInterval(timer);
  }, [id, runs, selectedRun]);

  const handleTrigger = useCallback(async () => {
    if (!id || triggering) return;
    setTriggering(true);
    try {
      const newRun = await triggerAutomation(id);
      setRuns((prev) => [newRun, ...prev]);
      setSelectedRun(newRun);
      refreshAutomations();
    } catch (e) {
      console.error("Failed to trigger automation:", e);
    } finally {
      setTriggering(false);
    }
  }, [id, triggering, refreshAutomations]);

  const handleToggleState = useCallback(async () => {
    if (!id || !automation) return;
    try {
      const newState: AutomationState = automation.state === "active" ? "paused" : "active";
      const updated = await updateAutomation(id, { state: newState });
      setAutomation(updated);
      refreshAutomations();
    } catch (e) {
      console.error("Failed to toggle automation state:", e);
    }
  }, [id, automation, refreshAutomations]);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (error || !automation) {
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
        <p className="text-muted-foreground mt-4">{error || "Automation not found."}</p>
      </div>
    );
  }

  const isRunning = runs.some((r) => r.status === "running" || r.status === "pending");

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

        <div className="flex items-center gap-2 shrink-0">
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
        {/* Main: tabbed content */}
        <Tabs defaultValue="output" className="flex-1 flex flex-col min-h-0 min-w-0">
          {/* Tab bar */}
          <div className="flex items-center border-b border-border px-4 shrink-0">
            <TabsList variant="line" className="h-10">
              <TabsTrigger value="output" className="gap-1.5 px-3">
                <MessageSquare className="h-3.5 w-3.5" />
                Output
              </TabsTrigger>
              <TabsTrigger value="details" className="gap-1.5 px-3">
                <FileText className="h-3.5 w-3.5" />
                Prompt
              </TabsTrigger>
            </TabsList>
          </div>

          {/* Output tab */}
          <TabsContent value="output" className="flex-1 min-h-0 flex">
            {/* Run history sidebar */}
            <div className="w-64 shrink-0 border-r border-border flex flex-col">
              <div className="px-3 py-2 border-b border-border">
                <h3 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">
                  Run History
                </h3>
              </div>
              <ScrollArea className="flex-1">
                <RunHistoryList
                  runs={runs}
                  selectedRunId={selectedRun?.id ?? null}
                  onSelectRun={setSelectedRun}
                />
              </ScrollArea>
            </div>

            {/* Run output view */}
            <div className="flex-1 flex flex-col min-h-0">
              {selectedRun ? (
                <>
                  {/* Run header */}
                  <div className="flex items-center gap-3 px-4 py-2 border-b border-border bg-muted/30">
                    <RunStatusBadge status={selectedRun.status} />
                    <span className="text-sm text-muted-foreground">
                      Started {formatRelativeTime(selectedRun.started_at)}
                    </span>
                    {selectedRun.completed_at && (
                      <span className="text-xs text-muted-foreground/70">
                        ({formatDuration(selectedRun.started_at, selectedRun.completed_at)})
                      </span>
                    )}
                  </div>
                  <RunOutputView run={selectedRun} />
                </>
              ) : (
                <div className="flex flex-col items-center justify-center flex-1 text-center px-4">
                  <Workflow className="h-12 w-12 text-muted-foreground mb-3" />
                  <p className="text-muted-foreground">Select a run to view output</p>
                  <p className="text-xs text-muted-foreground/70 mt-1">
                    Or click "Run Now" to start a new run
                  </p>
                </div>
              )}
            </div>
          </TabsContent>

          {/* Details tab */}
          <TabsContent value="details" className="flex-1 min-h-0 overflow-auto">
            <div className="p-4">
              <h3 className="text-sm font-medium mb-2">Prompt</h3>
              {automation.prompt ? (
                <div className="prose prose-sm prose-invert max-w-none">
                  <Markdown remarkPlugins={[remarkGfm]}>{automation.prompt}</Markdown>
                </div>
              ) : (
                <p className="text-muted-foreground text-sm">No prompt provided.</p>
              )}

              {automation.compiled_workflow && (
                <>
                  <Separator className="my-6" />
                  <h3 className="text-sm font-medium mb-2">Compiled Workflow</h3>
                  <pre className="text-xs font-mono bg-muted/50 rounded-md p-3 overflow-x-auto whitespace-pre-wrap">
                    {automation.compiled_workflow}
                  </pre>
                </>
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
              <PropertiesSidebar automation={automation} projectName={projectName} />
            </div>
          </ScrollArea>
        </div>
      </div>
    </div>
  );
}
