import { useEffect, useState, useCallback } from "react";
import { Link } from "react-router-dom";
import { Container, RefreshCw, Clock, Box, Activity } from "lucide-react";
import { fetchSessions } from "@/lib/api";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { Session, TaskState } from "@/lib/types";

// ---------------------------------------------------------------------------
// State badge (reuse task state styling)
// ---------------------------------------------------------------------------

const stateConfig: Record<TaskState, { label: string; className: string }> = {
  waiting: { label: "Waiting", className: "bg-slate-500/15 text-slate-400 border-slate-500/30" },
  blocked: { label: "Blocked", className: "bg-orange-500/15 text-orange-400 border-orange-500/30" },
  running: { label: "Running", className: "bg-green-500/15 text-green-400 border-green-500/30 animate-pulse" },
  question: { label: "Question", className: "bg-purple-500/15 text-purple-400 border-purple-500/30" },
  testing: { label: "Testing", className: "bg-cyan-500/15 text-cyan-400 border-cyan-500/30 animate-pulse" },
  awaiting_merge: { label: "Awaiting Merge", className: "bg-blue-500/15 text-blue-400 border-blue-500/30" },
  conflict: { label: "Conflict", className: "bg-red-500/15 text-red-400 border-red-500/30" },
  changes_requested: { label: "Changes Requested", className: "bg-amber-500/15 text-amber-400 border-amber-500/30" },
  completed: { label: "Completed", className: "bg-emerald-500/15 text-emerald-400 border-emerald-500/30" },
  failed: { label: "Failed", className: "bg-red-500/15 text-red-400 border-red-500/30" },
  cancelled: { label: "Cancelled", className: "bg-slate-500/15 text-slate-400 border-slate-500/30" },
};

function StateBadge({ state }: { state: TaskState }) {
  const config = stateConfig[state] ?? { label: state, className: "" };
  return (
    <Badge variant="outline" className={config.className}>
      {config.label}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Format uptime
// ---------------------------------------------------------------------------

function formatUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ${secs % 60}s`;
  const hours = Math.floor(mins / 60);
  const remainingMins = mins % 60;
  if (hours < 24) return `${hours}h ${remainingMins}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

// ---------------------------------------------------------------------------
// Containers Page
// ---------------------------------------------------------------------------

export function ContainersPage() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const loadSessions = useCallback(async (showRefreshing = false) => {
    if (showRefreshing) setRefreshing(true);
    try {
      const data = await fetchSessions();
      setSessions(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load sessions");
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, []);

  // Initial load
  useEffect(() => {
    loadSessions();
  }, [loadSessions]);

  // Auto-refresh every 5 seconds
  useEffect(() => {
    const interval = setInterval(() => loadSessions(), 5000);
    return () => clearInterval(interval);
  }, [loadSessions]);

  const handleRefresh = () => loadSessions(true);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center h-full py-32 gap-4">
        <p className="text-red-400 text-sm">{error}</p>
        <Button variant="outline" size="sm" onClick={handleRefresh}>
          Retry
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold">Containers</h1>
          <span className="text-xs text-muted-foreground">
            {sessions.length} active
          </span>
        </div>

        <Button
          variant="outline"
          size="sm"
          onClick={handleRefresh}
          disabled={refreshing}
          className="h-7 text-xs gap-1"
        >
          <RefreshCw className={cn("h-3.5 w-3.5", refreshing && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {/* Content */}
      <ScrollArea className="flex-1">
        {sessions.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-32 gap-4">
            <div className="flex h-12 w-12 items-center justify-center rounded-full bg-muted">
              <Box className="h-6 w-6 text-muted-foreground" />
            </div>
            <div className="text-center">
              <p className="text-sm font-medium">No active containers</p>
              <p className="text-xs text-muted-foreground mt-1">
                Containers will appear here when tasks are running
              </p>
            </div>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[140px]">Container</TableHead>
                <TableHead>Task</TableHead>
                <TableHead className="w-[140px]">State</TableHead>
                <TableHead className="w-[140px]">Project</TableHead>
                <TableHead className="w-[100px] text-right">Uptime</TableHead>
                <TableHead className="w-[100px] text-right">Started</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sessions.map((session) => (
                <TableRow key={session.container_id}>
                  {/* Container ID */}
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <Container className="h-4 w-4 text-muted-foreground shrink-0" />
                      <span className="font-mono text-xs text-muted-foreground truncate">
                        {session.container_id.slice(0, 12)}
                      </span>
                    </div>
                  </TableCell>

                  {/* Task title + link */}
                  <TableCell>
                    <Link
                      to={`/tasks/${session.task_id}`}
                      className="text-sm hover:underline truncate block max-w-[400px]"
                    >
                      {session.task_title}
                    </Link>
                  </TableCell>

                  {/* Task state */}
                  <TableCell>
                    <StateBadge state={session.task_state} />
                  </TableCell>

                  {/* Project */}
                  <TableCell>
                    <span className="text-sm text-muted-foreground">
                      {session.project_repo
                        ? session.project_repo.split("/")[1] ?? session.project_repo
                        : session.project_id.slice(0, 8)}
                    </span>
                  </TableCell>

                  {/* Uptime */}
                  <TableCell className="text-right">
                    <div className="flex items-center justify-end gap-1.5">
                      <Activity className="h-3.5 w-3.5 text-green-500" />
                      <span className="text-xs font-mono text-muted-foreground">
                        {formatUptime(session.uptime_secs)}
                      </span>
                    </div>
                  </TableCell>

                  {/* Started at */}
                  <TableCell className="text-right">
                    <span className="text-xs text-muted-foreground whitespace-nowrap">
                      {formatRelativeTime(session.started_at)}
                    </span>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </ScrollArea>
    </div>
  );
}
