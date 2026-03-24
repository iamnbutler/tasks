import { useEffect, useState, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { Box, Clock, ExternalLink, RefreshCw } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { fetchContainers } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import type { ContainerInfo, Task } from "@/lib/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatUptime(seconds: number): string {
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ${seconds % 60}s`;
  }
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  if (hours < 24) {
    return `${hours}h ${remainingMinutes}m`;
  }
  const days = Math.floor(hours / 24);
  const remainingHours = hours % 24;
  return `${days}d ${remainingHours}h`;
}

function getTaskTitle(tasks: Task[], taskId: string): string | null {
  const task = tasks.find((t) => t.id === taskId);
  return task?.title ?? null;
}

function getTaskState(tasks: Task[], taskId: string): string | null {
  const task = tasks.find((t) => t.id === taskId);
  return task?.state ?? null;
}

function stateColor(state: string | null): string {
  switch (state) {
    case "running":
      return "bg-green-500/15 text-green-400 border-green-500/30";
    case "question":
      return "bg-yellow-500/15 text-yellow-400 border-yellow-500/30";
    case "testing":
      return "bg-blue-500/15 text-blue-400 border-blue-500/30";
    default:
      return "bg-gray-500/15 text-gray-400 border-gray-500/30";
  }
}

// ---------------------------------------------------------------------------
// Containers page
// ---------------------------------------------------------------------------

export function ContainersPage() {
  const { snapshot } = useAppState();
  const navigate = useNavigate();
  const [containers, setContainers] = useState<ContainerInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const tasks = snapshot?.tasks ?? [];

  const loadContainers = useCallback(async () => {
    try {
      setError(null);
      const data = await fetchContainers();
      setContainers(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to fetch containers");
    } finally {
      setLoading(false);
    }
  }, []);

  // Initial load and periodic refresh
  useEffect(() => {
    loadContainers();
    const interval = setInterval(loadContainers, 5000);
    return () => clearInterval(interval);
  }, [loadContainers]);

  const handleRefresh = () => {
    setLoading(true);
    loadContainers();
  };

  const handleTaskClick = (taskId: string) => {
    navigate(`/tasks/${taskId}`);
  };

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold">Containers</h1>
          <Badge variant="outline" className="text-xs">
            {containers.length} active
          </Badge>
        </div>

        <Button
          size="sm"
          variant="outline"
          onClick={handleRefresh}
          disabled={loading}
          className="gap-1.5 h-7 text-xs"
        >
          <RefreshCw className={cn("h-3 w-3", loading && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {/* Containers table */}
      <ScrollArea className="flex-1">
        {error ? (
          <div className="flex items-center justify-center py-20">
            <p className="text-red-400 text-sm">{error}</p>
          </div>
        ) : containers.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-20 gap-3">
            <Box className="h-10 w-10 text-muted-foreground/50" />
            <p className="text-muted-foreground text-sm">
              {loading ? "Loading containers..." : "No active containers"}
            </p>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[200px]">Container ID</TableHead>
                <TableHead className="w-[100px]">Status</TableHead>
                <TableHead className="w-[120px]">Uptime</TableHead>
                <TableHead>Task</TableHead>
                <TableHead className="w-[100px]">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {containers.map((container) => {
                const taskTitle = getTaskTitle(tasks, container.task_id);
                const taskState = getTaskState(tasks, container.task_id);
                return (
                  <TableRow key={container.container_id}>
                    <TableCell className="font-mono text-xs">
                      {container.container_id.slice(0, 12)}
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant="outline"
                        className={cn("text-xs capitalize", stateColor(taskState))}
                      >
                        {taskState ?? "unknown"}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      <div className="flex items-center gap-1.5">
                        <Clock className="h-3 w-3" />
                        {formatUptime(container.uptime_secs)}
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col gap-0.5">
                        <span className="text-sm truncate max-w-[300px]" title={taskTitle ?? undefined}>
                          {taskTitle ?? "Unknown task"}
                        </span>
                        <span className="font-mono text-xs text-muted-foreground">
                          {container.task_id.slice(0, 8)}
                        </span>
                      </div>
                    </TableCell>
                    <TableCell>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 text-xs gap-1"
                        onClick={() => handleTaskClick(container.task_id)}
                      >
                        <ExternalLink className="h-3 w-3" />
                        View Task
                      </Button>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </ScrollArea>
    </div>
  );
}

export default ContainersPage;
