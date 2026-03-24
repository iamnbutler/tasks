import { useEffect, useState, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { Box, Clock, ExternalLink, RefreshCw } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { fetchContainers } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  ListView,
  ListEmptyState,
  ListErrorState,
} from "@/components/ui/list-view";
import { ListHeader, ListHeaderBadge } from "@/components/ui/list-header";
import {
  ListRow,
  IdCell,
  BadgeCell,
  TimeCell,
  TextCell,
  ActionsCell,
} from "@/components/ui/list-row";
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
// Container row component
// ---------------------------------------------------------------------------

function ContainerRow({
  container,
  tasks,
  onViewTask,
}: {
  container: ContainerInfo;
  tasks: Task[];
  onViewTask: (taskId: string) => void;
}) {
  const taskTitle = getTaskTitle(tasks, container.task_id);
  const taskState = getTaskState(tasks, container.task_id);

  return (
    <ListRow onRowClick={() => onViewTask(container.task_id)}>
      <IdCell width="w-28">{container.container_id.slice(0, 12)}</IdCell>
      <BadgeCell>
        <Badge
          variant="outline"
          className={cn("text-xs capitalize", stateColor(taskState))}
        >
          {taskState ?? "unknown"}
        </Badge>
      </BadgeCell>
      <TimeCell width="w-24" icon={<Clock className="h-3 w-3" />}>
        {formatUptime(container.uptime_secs)}
      </TimeCell>
      <TextCell>
        <span className="truncate" title={taskTitle ?? undefined}>
          {taskTitle ?? "Unknown task"}
        </span>
      </TextCell>
      <IdCell width="w-20">{container.task_id.slice(0, 8)}</IdCell>
      <ActionsCell>
        <Button
          size="sm"
          variant="ghost"
          className="h-7 text-xs gap-1"
          onClick={() => onViewTask(container.task_id)}
        >
          <ExternalLink className="h-3 w-3" />
          View
        </Button>
      </ActionsCell>
    </ListRow>
  );
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

  const headerActions = (
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
  );

  const renderContent = () => {
    if (error) {
      return <ListErrorState message={error} onRetry={handleRefresh} />;
    }

    if (containers.length === 0) {
      return (
        <ListEmptyState
          icon={<Box className="h-6 w-6 text-muted-foreground" />}
          message={loading ? "Loading containers..." : "No active containers"}
        />
      );
    }

    return (
      <div>
        {containers.map((container) => (
          <ContainerRow
            key={container.container_id}
            container={container}
            tasks={tasks}
            onViewTask={handleTaskClick}
          />
        ))}
      </div>
    );
  };

  return (
    <ListView
      header={
        <ListHeader
          title="Containers"
          actions={
            <div className="flex items-center gap-2">
              <ListHeaderBadge count={containers.length} label="active" />
              {headerActions}
            </div>
          }
        />
      }
    >
      {renderContent()}
    </ListView>
  );
}

export default ContainersPage;
