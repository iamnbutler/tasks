import { useState, useEffect, useCallback } from "react";
import { Link } from "react-router-dom";
import {
  AlertCircle,
  Check,
  ChevronDown,
  ChevronRight,
  Clock,
  ExternalLink,
  Loader2,
  X,
} from "lucide-react";
import { fetchAutomationRuns } from "@/lib/api";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import type { Automation, AutomationRun } from "@/lib/types";

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
      <Icon
        className={cn("h-3 w-3", isRunning && "animate-spin")}
      />
      {config.label}
    </Badge>
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
// Run detail view
// ---------------------------------------------------------------------------

function RunDetailView({ run }: { run: AutomationRun }) {
  const hasOutput = run.output && run.output.trim().length > 0;
  const hasError = run.error && run.error.trim().length > 0;

  return (
    <div className="space-y-3 pt-2 pb-1">
      {/* Timestamps */}
      <div className="flex flex-wrap gap-4 text-xs text-muted-foreground">
        <div>
          <span className="font-medium text-foreground/80">Started:</span>{" "}
          <span title={new Date(run.started_at).toLocaleString()}>
            {formatRelativeTime(run.started_at)}
          </span>
        </div>
        {run.completed_at && (
          <div>
            <span className="font-medium text-foreground/80">Completed:</span>{" "}
            <span title={new Date(run.completed_at).toLocaleString()}>
              {formatRelativeTime(run.completed_at)}
            </span>
          </div>
        )}
        <div>
          <span className="font-medium text-foreground/80">Duration:</span>{" "}
          {formatDuration(run.started_at, run.completed_at)}
        </div>
      </div>

      {/* Error message */}
      {hasError && (
        <div className="space-y-1">
          <div className="text-xs font-medium text-red-400">Error</div>
          <div className="rounded-md border border-red-500/30 bg-red-500/10 p-2">
            <pre className="text-xs text-red-300 whitespace-pre-wrap break-words font-mono">
              {run.error}
            </pre>
          </div>
        </div>
      )}

      {/* Output */}
      {hasOutput && (
        <Collapsible defaultOpen={run.status === "completed" && !hasError}>
          <div className="space-y-1">
            <CollapsibleTrigger className="flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors group">
              <ChevronRight className="h-3 w-3 transition-transform group-data-[state=open]:rotate-90" />
              Output
            </CollapsibleTrigger>
            <CollapsibleContent>
              <div className="max-h-60 overflow-auto rounded-md border border-border bg-muted/50">
                <pre className="p-2 text-xs whitespace-pre-wrap break-words font-mono">
                  {run.output}
                </pre>
              </div>
            </CollapsibleContent>
          </div>
        </Collapsible>
      )}

      {/* No output */}
      {!hasOutput && !hasError && run.status === "completed" && (
        <div className="text-xs text-muted-foreground italic">
          No output recorded
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Run row
// ---------------------------------------------------------------------------

function RunRow({ run, automationId }: { run: AutomationRun; automationId: string }) {
  const [expanded, setExpanded] = useState(false);

  // Auto-expand if this is a running or recently failed run
  useEffect(() => {
    if (run.status === "running" || run.status === "failed") {
      setExpanded(true);
    }
  }, [run.status]);

  return (
    <Collapsible open={expanded} onOpenChange={setExpanded}>
      <div className="border-b border-border last:border-b-0">
        <div className="flex items-center gap-3 w-full px-3 py-2 hover:bg-accent/30 transition-colors text-left">
          <CollapsibleTrigger className="p-0 border-0 bg-transparent">
            <ChevronRight
              className={cn(
                "h-3.5 w-3.5 text-muted-foreground transition-transform shrink-0",
                expanded && "rotate-90"
              )}
            />
          </CollapsibleTrigger>
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
          <Link
            to={`/automations/${automationId}/runs/${run.id}`}
            className="p-1 rounded hover:bg-accent/50 transition-colors"
            title="View run details"
          >
            <ExternalLink className="h-3.5 w-3.5 text-muted-foreground hover:text-foreground" />
          </Link>
        </div>
        <CollapsibleContent>
          <div className="px-3 pb-2 pl-9">
            <RunDetailView run={run} />
          </div>
        </CollapsibleContent>
      </div>
    </Collapsible>
  );
}

// ---------------------------------------------------------------------------
// AutomationRunsPanel
// ---------------------------------------------------------------------------

interface AutomationRunsPanelProps {
  automation: Automation;
  onClose: () => void;
}

export function AutomationRunsPanel({
  automation,
  onClose,
}: AutomationRunsPanelProps) {
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const loadRuns = useCallback(async () => {
    try {
      const data = await fetchAutomationRuns(automation.id);
      // Sort by started_at descending (most recent first)
      const sorted = [...data].sort(
        (a, b) =>
          new Date(b.started_at).getTime() - new Date(a.started_at).getTime()
      );
      setRuns(sorted);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load runs");
    } finally {
      setLoading(false);
    }
  }, [automation.id]);

  // Initial load + regular polling
  // Always poll so new runs appear even if triggered externally.
  // Poll faster (2s) when a run is in progress, otherwise every 5s.
  useEffect(() => {
    loadRuns();
    const hasRunningRun = runs.some((r) => r.status === "running");
    const interval = setInterval(loadRuns, hasRunningRun ? 2000 : 5000);
    return () => clearInterval(interval);
  }, [runs, loadRuns]);

  return (
    <div className="flex flex-col h-full border-l border-border bg-background">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5 shrink-0">
        <div className="flex items-center gap-2 min-w-0">
          <h2 className="text-sm font-semibold truncate">{automation.name}</h2>
          <span className="text-xs text-muted-foreground shrink-0">
            Run History
          </span>
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="h-6 w-6 shrink-0"
          onClick={onClose}
        >
          <X className="h-4 w-4" />
        </Button>
      </div>

      {/* Content */}
      <ScrollArea className="flex-1">
        {loading ? (
          <div className="flex items-center justify-center py-12">
            <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
          </div>
        ) : error ? (
          <div className="flex flex-col items-center justify-center py-12 px-4 text-center">
            <AlertCircle className="h-8 w-8 text-red-400 mb-2" />
            <p className="text-sm text-red-400">{error}</p>
            <Button
              variant="outline"
              size="sm"
              className="mt-3"
              onClick={() => {
                setLoading(true);
                loadRuns();
              }}
            >
              Retry
            </Button>
          </div>
        ) : runs.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-12 px-4 text-center">
            <Clock className="h-8 w-8 text-muted-foreground mb-2" />
            <p className="text-sm text-muted-foreground">No runs yet</p>
            <p className="text-xs text-muted-foreground/70 mt-1">
              Trigger this automation to see run history
            </p>
          </div>
        ) : (
          <div>
            {runs.map((run) => (
              <RunRow key={run.id} run={run} automationId={automation.id} />
            ))}
          </div>
        )}
      </ScrollArea>

      {/* Footer with run count */}
      {!loading && !error && runs.length > 0 && (
        <div className="border-t border-border px-4 py-2 text-xs text-muted-foreground shrink-0">
          {runs.length} {runs.length === 1 ? "run" : "runs"}
        </div>
      )}
    </div>
  );
}
