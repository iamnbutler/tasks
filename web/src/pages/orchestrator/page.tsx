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
import { cn, formatRelativeTime } from "@/lib/utils";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// Parse orchestrator events into displayable blocks
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
}

function parseOrchestratorEvents(events: Event[]): OrchestratorBlock[] {
  const blocks: OrchestratorBlock[] = [];

  for (const event of events) {
    if (event.type === "orchestrator:decision") {
      blocks.push({
        kind: "decision",
        id: event.id,
        timestamp: event.ts,
        approved: event.data?.approved === true,
        reasoning: typeof event.data?.reasoning === "string" ? event.data.reasoning : undefined,
        taskId: typeof event.data?.task_id === "string" ? event.data.task_id : event.task,
        entryId: typeof event.data?.entry_id === "string" ? event.data.entry_id : undefined,
      });
      continue;
    }

    if (event.type === "orchestrator:feedback") {
      blocks.push({
        kind: "feedback",
        id: event.id,
        timestamp: event.ts,
        feedback: typeof event.data?.feedback === "string" ? event.data.feedback : undefined,
        taskId: typeof event.data?.task_id === "string" ? event.data.task_id : event.task,
        content: typeof event.data?.context === "string" ? event.data.context : undefined,
      });
      continue;
    }

    if (event.type === "orchestrator:escalation") {
      blocks.push({
        kind: "escalation",
        id: event.id,
        timestamp: event.ts,
        content: typeof event.data?.reason === "string" ? event.data.reason : JSON.stringify(event.data),
        taskId: event.task,
      });
      continue;
    }

    if (event.type === "orchestrator:message") {
      blocks.push({
        kind: "message",
        id: event.id,
        timestamp: event.ts,
        content: typeof event.data?.message === "string" ? event.data.message : undefined,
        actor: event.actor,
      });
      continue;
    }
  }

  return blocks;
}

// ---------------------------------------------------------------------------
// Block display components
// ---------------------------------------------------------------------------

function DecisionBlock({ block }: { block: OrchestratorBlock }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div
      className={cn(
        "rounded-md border p-3",
        block.approved
          ? "border-green-500/30 bg-green-500/10"
          : "border-red-500/30 bg-red-500/10"
      )}
    >
      <div className="flex items-start gap-2">
        {block.approved ? (
          <CheckCircle2 className="h-4 w-4 text-green-500 mt-0.5 shrink-0" />
        ) : (
          <XCircle className="h-4 w-4 text-red-500 mt-0.5 shrink-0" />
        )}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className={cn("font-medium text-sm", block.approved ? "text-green-400" : "text-red-400")}>
              {block.approved ? "Approved" : "Rejected"}
            </span>
            {block.taskId && (
              <Badge variant="outline" className="font-mono text-xs">
                task:{block.taskId.slice(0, 8)}
              </Badge>
            )}
            <span className="text-xs text-muted-foreground">
              {formatRelativeTime(block.timestamp)}
            </span>
          </div>
          {block.reasoning && (
            <button
              onClick={() => setExpanded(!expanded)}
              className="mt-1 text-sm text-muted-foreground hover:text-foreground flex items-center gap-1"
            >
              <ChevronDown className={cn("h-3 w-3 transition-transform", expanded && "rotate-180")} />
              {expanded ? "Hide" : "Show"} reasoning
            </button>
          )}
          {expanded && block.reasoning && (
            <p className="mt-2 text-sm text-muted-foreground whitespace-pre-wrap">
              {block.reasoning}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function FeedbackBlock({ block }: { block: OrchestratorBlock }) {
  return (
    <div className="rounded-md border border-blue-500/30 bg-blue-500/10 p-3">
      <div className="flex items-start gap-2">
        <MessageSquare className="h-4 w-4 text-blue-500 mt-0.5 shrink-0" />
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium text-sm text-blue-400">Feedback</span>
            {block.taskId && (
              <Badge variant="outline" className="font-mono text-xs">
                task:{block.taskId.slice(0, 8)}
              </Badge>
            )}
            {block.content && (
              <Badge variant="outline" className="text-xs">
                {block.content}
              </Badge>
            )}
            <span className="text-xs text-muted-foreground">
              {formatRelativeTime(block.timestamp)}
            </span>
          </div>
          {block.feedback && (
            <p className="mt-2 text-sm text-muted-foreground whitespace-pre-wrap">
              {block.feedback}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function EscalationBlock({ block }: { block: OrchestratorBlock }) {
  return (
    <div className="rounded-md border border-yellow-500/30 bg-yellow-500/10 p-3">
      <div className="flex items-start gap-2">
        <AlertTriangle className="h-4 w-4 text-yellow-500 mt-0.5 shrink-0" />
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium text-sm text-yellow-400">Escalation</span>
            {block.taskId && (
              <Badge variant="outline" className="font-mono text-xs">
                task:{block.taskId.slice(0, 8)}
              </Badge>
            )}
            <span className="text-xs text-muted-foreground">
              {formatRelativeTime(block.timestamp)}
            </span>
          </div>
          {block.content && (
            <p className="mt-2 text-sm text-muted-foreground whitespace-pre-wrap">
              {block.content}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function MessageBlock({ block }: { block: OrchestratorBlock }) {
  const isHuman = block.actor === "Human";

  return (
    <div
      className={cn(
        "rounded-md border p-3",
        isHuman
          ? "border-purple-500/30 bg-purple-500/10"
          : "border-orange-500/30 bg-orange-500/10"
      )}
    >
      <div className="flex items-start gap-2">
        <Brain className={cn("h-4 w-4 mt-0.5 shrink-0", isHuman ? "text-purple-500" : "text-orange-500")} />
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 flex-wrap">
            <span className={cn("font-medium text-sm", isHuman ? "text-purple-400" : "text-orange-400")}>
              {isHuman ? "You" : "Orchestrator"}
            </span>
            <span className="text-xs text-muted-foreground">
              {formatRelativeTime(block.timestamp)}
            </span>
          </div>
          {block.content && (
            <p className="mt-1 text-sm whitespace-pre-wrap">
              {block.content}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function BlockView({ block }: { block: OrchestratorBlock }) {
  switch (block.kind) {
    case "decision":
      return <DecisionBlock block={block} />;
    case "feedback":
      return <FeedbackBlock block={block} />;
    case "escalation":
      return <EscalationBlock block={block} />;
    case "message":
      return <MessageBlock block={block} />;
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Orchestrator Chat View
// ---------------------------------------------------------------------------

function OrchestratorView() {
  const { events: allEvents } = useAppState();
  const [localEvents, setLocalEvents] = useState<Event[]>([]);
  const [chatInput, setChatInput] = useState("");
  const [sending, setSending] = useState(false);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [hasNewMessages, setHasNewMessages] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  // Filter orchestrator events from global events and merge with local
  const orchestratorEvents = useMemo(() => {
    const globalOrchEvents = allEvents.filter(
      (e) => e.type.startsWith("orchestrator:")
    );
    // Merge and deduplicate
    const merged = [...globalOrchEvents, ...localEvents];
    const seen = new Set<string>();
    return merged
      .filter((e) => {
        if (seen.has(e.id)) return false;
        seen.add(e.id);
        return true;
      })
      .sort((a, b) => a.ts.localeCompare(b.ts));
  }, [allEvents, localEvents]);

  // Subscribe to orchestrator events via SSE
  useEffect(() => {
    const source = subscribeEvents({ pattern: "orchestrator:*" });
    source.onmessage = (msg) => {
      try {
        const event: Event = JSON.parse(msg.data);
        if (event.type.startsWith("orchestrator:")) {
          setLocalEvents((prev) => [...prev, event]);
        }
      } catch {
        // ignore parse errors
      }
    };
    return () => source.close();
  }, []);

  const blocks = parseOrchestratorEvents(orchestratorEvents);
  const prevBlocksLength = useRef(blocks.length);

  // Check if user is scrolled to bottom
  const checkIfAtBottom = useCallback(() => {
    const container = scrollContainerRef.current;
    if (!container) return true;
    const threshold = 50;
    return container.scrollHeight - container.scrollTop - container.clientHeight <= threshold;
  }, []);

  const handleScroll = useCallback(() => {
    const atBottom = checkIfAtBottom();
    setIsAtBottom(atBottom);
    if (atBottom) {
      setHasNewMessages(false);
    }
  }, [checkIfAtBottom]);

  // Auto-scroll when new messages arrive
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

  return (
    <Card className="flex flex-col flex-1 min-h-0">
      <CardHeader className="pb-2 shrink-0">
        <CardTitle className="text-sm font-medium flex items-center gap-2">
          <Brain className="h-4 w-4 text-orange-500" />
          Orchestrator Activity
        </CardTitle>
      </CardHeader>
      <CardContent className="flex flex-col flex-1 min-h-0 gap-2">
        {/* Message stream */}
        <div className="relative flex-1 min-h-0">
          <div
            ref={scrollContainerRef}
            onScroll={handleScroll}
            className="absolute inset-0 overflow-y-auto rounded-md border border-border bg-background p-3 space-y-2"
          >
            {blocks.length === 0 && (
              <div className="flex flex-col items-center justify-center h-full py-8 text-center">
                <Brain className="h-12 w-12 text-muted-foreground/50 mb-4" />
                <p className="text-muted-foreground text-sm">
                  No orchestrator activity yet.
                </p>
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
          {/* New messages indicator */}
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
          className="flex gap-2 shrink-0"
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
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Orchestrator Page
// ---------------------------------------------------------------------------

export function OrchestratorPage() {
  const { snapshot } = useAppState();

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full p-4">
      <OrchestratorView />
    </div>
  );
}

export default OrchestratorPage;
