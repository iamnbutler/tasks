import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Send,
  ChevronDown,
  CheckCircle2,
  XCircle,
  MessageSquare,
  AlertTriangle,
  Brain,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { sendOrchestratorChat, subscribeEvents } from "@/lib/api";
import { cn, formatRelativeTime, projectLabel } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { Event, Task, Project } from "@/lib/types";

// ---------------------------------------------------------------------------
// Task helpers: resolve task ID → title, number, repo info
// ---------------------------------------------------------------------------

interface TaskInfo {
  taskId: string;
  number: string;       // "#123" or truncated ID
  title: string;        // Full title from task
  repoName: string;     // Just repo name (e.g., "tasks")
  fullRepo: string;     // Full owner/repo (e.g., "owner/tasks")
  prUrl?: string;       // PR URL if available
}

function getTaskInfo(taskId: string | undefined, tasks: Task[], projects: Project[]): TaskInfo | null {
  if (!taskId) return null;
  const task = tasks.find((t) => t.id === taskId);
  if (!task) {
    return {
      taskId,
      number: taskId.slice(0, 8),
      title: "Unknown task",
      repoName: "unknown",
      fullRepo: "unknown",
    };
  }
  const { source } = task;
  const number =
    (source.type === "github_issue" || source.type === "github_pr")
      ? `#${source.number}`
      : taskId.slice(0, 8);
  const fullRepo = projectLabel(task.project, projects);
  const repoName = fullRepo.includes("/") ? fullRepo.split("/")[1]! : fullRepo;
  return {
    taskId,
    number,
    title: task.title,
    repoName,
    fullRepo,
  };
}

// ---------------------------------------------------------------------------
// Parse orchestrator events
// ---------------------------------------------------------------------------

interface OrchestratorBlock {
  kind: "decision" | "feedback" | "escalation" | "message" | "system";
  id: string;
  timestamp: string;
  approved?: boolean;
  reasoning?: string;
  feedback?: string;
  taskId?: string;
  entryId?: string;
  content?: string;
  actor?: string;
  /** Resolved task info for richer display */
  taskInfo?: TaskInfo;
  /** Escalation-specific fields */
  escalationAction?: "conflict_needs_human" | "mode_lowered" | string;
  prUrl?: string;
  fromMode?: string;
  toMode?: string;
}

function parseOrchestratorEvents(
  events: Event[],
  tasks: Task[],
  projects: Project[]
): OrchestratorBlock[] {
  const blocks: OrchestratorBlock[] = [];
  for (const event of events) {
    if (event.type === "orchestrator:decision") {
      const taskId = typeof event.data?.task_id === "string" ? event.data.task_id : event.task;
      blocks.push({
        kind: "decision",
        id: event.id,
        timestamp: event.ts,
        approved: event.data?.approved === true,
        reasoning: typeof event.data?.reasoning === "string" ? event.data.reasoning : undefined,
        taskId,
        taskInfo: getTaskInfo(taskId, tasks, projects) ?? undefined,
        entryId: typeof event.data?.entry_id === "string" ? event.data.entry_id : undefined,
      });
    } else if (event.type === "orchestrator:feedback") {
      const taskId = typeof event.data?.task_id === "string" ? event.data.task_id : event.task;
      blocks.push({
        kind: "feedback",
        id: event.id,
        timestamp: event.ts,
        feedback: typeof event.data?.feedback === "string" ? event.data.feedback : undefined,
        taskId,
        taskInfo: getTaskInfo(taskId, tasks, projects) ?? undefined,
        content: typeof event.data?.context === "string" ? event.data.context : undefined,
      });
    } else if (event.type === "orchestrator:escalation") {
      const action = typeof event.data?.action === "string" ? event.data.action : undefined;
      // Extract reasoning - backend sends "reasoning" for conflicts, "reason" for mode changes
      const reasoning =
        typeof event.data?.reasoning === "string" ? event.data.reasoning :
        typeof event.data?.reason === "string" ? event.data.reason :
        undefined;
      const taskId = event.task;

      blocks.push({
        kind: "escalation",
        id: event.id,
        timestamp: event.ts,
        content: reasoning,
        taskId,
        taskInfo: getTaskInfo(taskId, tasks, projects) ?? undefined,
        entryId: typeof event.data?.entry_id === "string" ? event.data.entry_id : undefined,
        escalationAction: action,
        prUrl: typeof event.data?.pr_url === "string" ? event.data.pr_url : undefined,
        fromMode: typeof event.data?.from === "string" ? event.data.from : undefined,
        toMode: typeof event.data?.to === "string" ? event.data.to : undefined,
      });
    } else if (event.type === "orchestrator:message") {
      // Human message to orchestrator
      blocks.push({
        kind: "message",
        id: event.id,
        timestamp: event.ts,
        content: typeof event.data?.message === "string" ? event.data.message : undefined,
        actor: "human",
      });
    } else if (event.type === "orchestrator:response") {
      // Orchestrator response to human
      blocks.push({
        kind: "message",
        id: event.id,
        timestamp: event.ts,
        content: typeof event.data?.message === "string" ? event.data.message : undefined,
        actor: "orchestrator",
      });
    }
  }
  return blocks;
}

// ---------------------------------------------------------------------------
// Block components
// ---------------------------------------------------------------------------

function DecisionBlock({ block }: { block: OrchestratorBlock }) {
  const [collapsed, setCollapsed] = useState(false);
  const info = block.taskInfo;

  // Build a conversational header message
  let headerText: string;
  if (block.approved) {
    if (info) {
      headerText = `Approving "${info.title}" (${info.number}) in ${info.repoName} to merge.`;
    } else {
      headerText = "Approving merge request.";
    }
  } else {
    if (info) {
      headerText = `Rejecting "${info.title}" (${info.number}) in ${info.repoName}.`;
    } else {
      headerText = "Rejecting merge request.";
    }
  }

  return (
    <div className="rounded-md border border-border/50 bg-card/50 p-4">
      <div className="flex items-start gap-3">
        <div className={cn(
          "rounded-full p-1.5 shrink-0",
          block.approved ? "bg-green-500/20" : "bg-red-500/20"
        )}>
          {block.approved ? (
            <CheckCircle2 className="h-4 w-4 text-green-500" />
          ) : (
            <XCircle className="h-4 w-4 text-red-500" />
          )}
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center justify-between gap-2">
            <span className="text-xs text-muted-foreground">{formatRelativeTime(block.timestamp)}</span>
          </div>
          <p className="mt-1 text-sm leading-relaxed">{headerText}</p>
          {block.reasoning && (
            <>
              <button
                onClick={() => setCollapsed(!collapsed)}
                className="mt-2 text-xs text-muted-foreground hover:text-foreground flex items-center gap-1"
              >
                <ChevronDown className={cn("h-3 w-3 transition-transform", collapsed && "-rotate-90")} />
                {collapsed ? "Show" : "Hide"} reasoning
              </button>
              {!collapsed && (
                <p className="mt-2 text-sm text-muted-foreground leading-relaxed whitespace-pre-wrap">
                  {block.reasoning}
                </p>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function FeedbackBlock({ block }: { block: OrchestratorBlock }) {
  const info = block.taskInfo;

  // Build a conversational header
  let headerText: string;
  if (info) {
    const contextLabel = block.content ? ` (${block.content})` : "";
    headerText = `Sending feedback to "${info.title}" (${info.number})${contextLabel}:`;
  } else {
    headerText = "Sending feedback to agent:";
  }

  return (
    <div className="rounded-md border border-border/50 bg-card/50 p-4">
      <div className="flex items-start gap-3">
        <div className="rounded-full p-1.5 bg-blue-500/20 shrink-0">
          <MessageSquare className="h-4 w-4 text-blue-500" />
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center justify-between gap-2">
            <span className="text-xs text-muted-foreground">{formatRelativeTime(block.timestamp)}</span>
          </div>
          <p className="mt-1 text-sm leading-relaxed">{headerText}</p>
          {block.feedback && (
            <p className="mt-2 text-sm text-muted-foreground leading-relaxed whitespace-pre-wrap">
              {block.feedback}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function EscalationBlock({ block }: { block: OrchestratorBlock }) {
  const [collapsed, setCollapsed] = useState(false);
  const info = block.taskInfo;

  // Build a conversational message based on escalation type
  let headerText: string;
  let actionHint: string | null = null;

  if (block.escalationAction === "mode_lowered") {
    // Mode was lowered due to errors
    const modeTransition = block.fromMode && block.toMode
      ? ` from ${block.fromMode} to ${block.toMode}`
      : "";
    headerText = `I've lowered the system mode${modeTransition} due to detected issues.`;
    if (block.content) {
      // The content often contains the reason like "3 agent errors in tracking window"
      actionHint = `Reason: ${block.content}. You can review the recent activity and raise the mode when ready.`;
    } else {
      actionHint = "Review recent activity and raise the mode when ready to continue.";
    }
  } else if (block.escalationAction === "conflict_needs_human") {
    // Conflict needs human review
    if (info) {
      headerText = `"${info.title}" (${info.number}) in ${info.repoName} has a merge conflict that needs your attention.`;
    } else {
      headerText = "A merge conflict needs your attention.";
    }
    actionHint = block.prUrl
      ? "Please review the PR and resolve the conflict manually."
      : "Please resolve the conflict manually.";
  } else {
    // Generic escalation
    if (info) {
      headerText = `An issue with "${info.title}" (${info.number}) requires your attention.`;
    } else {
      headerText = "An issue requires your attention.";
    }
  }

  return (
    <div className="rounded-md border border-yellow-500/30 bg-yellow-500/10 p-4">
      <div className="flex items-start gap-3">
        <div className="rounded-full p-1.5 bg-yellow-500/20 shrink-0">
          <AlertTriangle className="h-4 w-4 text-yellow-500" />
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center justify-between gap-2">
            <span className="text-xs text-muted-foreground">{formatRelativeTime(block.timestamp)}</span>
            {block.prUrl && (
              <a
                href={block.prUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="text-xs text-blue-400 hover:underline"
              >
                View PR
              </a>
            )}
          </div>
          <p className="mt-1 text-sm leading-relaxed">{headerText}</p>
          {actionHint && (
            <p className="mt-1 text-sm text-muted-foreground leading-relaxed">{actionHint}</p>
          )}
          {block.content && block.escalationAction !== "mode_lowered" && (
            <>
              <button
                onClick={() => setCollapsed(!collapsed)}
                className="mt-2 text-xs text-muted-foreground hover:text-foreground flex items-center gap-1"
              >
                <ChevronDown className={cn("h-3 w-3 transition-transform", collapsed && "-rotate-90")} />
                {collapsed ? "Show" : "Hide"} details
              </button>
              {!collapsed && (
                <p className="mt-2 text-sm text-muted-foreground leading-relaxed whitespace-pre-wrap">
                  {block.content}
                </p>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function MessageBlock({ block }: { block: OrchestratorBlock }) {
  const isHuman = block.actor === "human";
  return (
    <div className="rounded-md border border-border/50 bg-card/50 p-4">
      <div className="flex items-start gap-3">
        <div className={cn(
          "rounded-full p-1.5 shrink-0",
          isHuman ? "bg-purple-500/20" : "bg-orange-500/20"
        )}>
          <Brain className={cn("h-4 w-4", isHuman ? "text-purple-500" : "text-orange-500")} />
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <span className={cn("text-sm font-medium", isHuman ? "text-purple-400" : "text-orange-400")}>
              {isHuman ? "You" : "Orchestrator"}
            </span>
            <span className="text-xs text-muted-foreground">{formatRelativeTime(block.timestamp)}</span>
          </div>
          {block.content && (
            <p className="mt-1 text-sm leading-relaxed whitespace-pre-wrap">{block.content}</p>
          )}
        </div>
      </div>
    </div>
  );
}

function BlockView({ block }: { block: OrchestratorBlock }) {
  switch (block.kind) {
    case "decision": return <DecisionBlock block={block} />;
    case "feedback": return <FeedbackBlock block={block} />;
    case "escalation": return <EscalationBlock block={block} />;
    case "message": return <MessageBlock block={block} />;
    default: return null;
  }
}

// ---------------------------------------------------------------------------
// Orchestrator Page
// ---------------------------------------------------------------------------

export function OrchestratorPage() {
  const { snapshot, events: allEvents } = useAppState();
  const [localEvents, setLocalEvents] = useState<Event[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [sending, setSending] = useState(false);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  const orchestratorEvents = useMemo(() => {
    const globalOrchEvents = allEvents.filter((e) => e.type.startsWith("orchestrator:"));
    const merged = [...globalOrchEvents, ...localEvents];
    const seen = new Set<string>();
    return merged
      .filter((e) => { if (seen.has(e.id)) return false; seen.add(e.id); return true; })
      .sort((a, b) => a.ts.localeCompare(b.ts));
  }, [allEvents, localEvents]);

  useEffect(() => {
    const source = subscribeEvents({ pattern: "orchestrator:*" });
    source.onmessage = (msg) => {
      try {
        const event: Event = JSON.parse(msg.data);
        if (event.type.startsWith("orchestrator:")) {
          setLocalEvents((prev) => [...prev, event]);
        }
      } catch { /* ignore */ }
    };
    return () => source.close();
  }, []);

  const tasks = snapshot?.tasks ?? [];
  const projects = snapshot?.projects ?? [];
  const blocks = useMemo(
    () => parseOrchestratorEvents(orchestratorEvents, tasks, projects),
    [orchestratorEvents, tasks, projects]
  );
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
      await sendOrchestratorChat(text);
    } catch (e) {
      console.error("Failed to send orchestrator message:", e);
    } finally {
      setSending(false);
    }
  }, [chatInput, sending]);

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-2">
          <Brain className="h-4 w-4 text-orange-500" />
          <h1 className="text-sm font-semibold">Orchestrator</h1>
        </div>
      </div>

      {/* Message stream */}
      <div className="relative flex-1 min-h-0">
        <div
          ref={scrollContainerRef}
          onScroll={handleScroll}
          className="absolute inset-0 overflow-y-auto p-4 space-y-3"
        >
          {blocks.length === 0 && (
            <div className="flex flex-col items-center justify-center h-full py-8 text-center">
              <Brain className="h-12 w-12 text-muted-foreground/50 mb-4" />
              <p className="text-muted-foreground text-sm">No orchestrator activity yet.</p>
              <p className="text-muted-foreground text-xs mt-1">
                The orchestrator evaluates merge queue entries and coordinates tasks.
              </p>
            </div>
          )}
          {blocks.map((block) => (
            <BlockView key={block.id} block={block} />
          ))}
          <div ref={bottomRef} />
        </div>
        {hasNewMessages && (
          <button
            onClick={scrollToBottom}
            className="absolute bottom-3 left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 rounded-full bg-orange-600 text-white text-sm font-medium shadow-lg hover:bg-orange-700 transition-colors"
          >
            <ChevronDown className="h-4 w-4" />
            New activity
          </button>
        )}
      </div>

      {/* Chat input */}
      <form
        className="flex gap-2 p-3 border-t border-border shrink-0"
        onSubmit={(e) => {
          e.preventDefault();
          handleSend();
        }}
      >
        <Input
          value={chatInput}
          onChange={(e) => setChatInput(e.target.value)}
          placeholder="Send a message to the orchestrator..."
          className="flex-1"
          disabled={sending}
        />
        <Button type="submit" size="icon" disabled={sending || !chatInput.trim()}>
          <Send className="h-4 w-4" />
        </Button>
      </form>
    </div>
  );
}

export default OrchestratorPage;
