import { useState } from "react";
import {
  Calendar,
  Clock,
  MoreHorizontal,
  Pause,
  Play,
  PlayCircle,
  Plus,
  Trash2,
  Workflow,
  Zap,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { deleteAutomation, triggerAutomation, updateAutomation } from "@/lib/api";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { Automation, AutomationState } from "@/lib/types";

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
// Automation row
// ---------------------------------------------------------------------------

function AutomationRow({
  automation,
  projectName,
  onRefresh,
}: {
  automation: Automation;
  projectName: string;
  onRefresh: () => Promise<void>;
}) {
  const [running, setRunning] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [deleting, setDeleting] = useState(false);

  async function handleRun() {
    setRunning(true);
    try {
      await triggerAutomation(automation.id);
      await onRefresh();
    } catch (error) {
      console.error("Failed to run automation:", error);
    } finally {
      setRunning(false);
    }
  }

  async function handleToggleState() {
    try {
      const newState: AutomationState = automation.state === "active" ? "paused" : "active";
      await updateAutomation(automation.id, { state: newState });
      await onRefresh();
    } catch (error) {
      console.error("Failed to toggle automation state:", error);
    }
  }

  async function handleDelete() {
    setDeleting(true);
    try {
      await deleteAutomation(automation.id);
      await onRefresh();
      setDeleteOpen(false);
    } catch (error) {
      console.error("Failed to delete automation:", error);
    } finally {
      setDeleting(false);
    }
  }

  return (
    <>
      <div className="flex items-center gap-4 px-4 py-3 border-b border-border hover:bg-accent/30 transition-colors">
        {/* Icon */}
        <div className="flex h-8 w-8 items-center justify-center rounded-md bg-accent shrink-0">
          <Workflow className="h-4 w-4 text-muted-foreground" />
        </div>

        {/* Name and trigger */}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-medium text-sm truncate">{automation.name}</span>
            <StateBadge state={automation.state} />
          </div>
          <TriggerDisplay trigger={automation.trigger} />
        </div>

        {/* Project */}
        <span className="text-xs text-muted-foreground shrink-0">{projectName}</span>

        {/* Updated */}
        <div className="flex items-center gap-1 text-xs text-muted-foreground shrink-0 w-20">
          <Clock className="h-3 w-3" />
          <span>{formatRelativeTime(automation.updated_at)}</span>
        </div>

        {/* Actions */}
        <div className="flex items-center gap-1 shrink-0">
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            onClick={handleRun}
            disabled={running || automation.state === "disabled"}
            title="Run now"
          >
            <Play className={cn("h-3.5 w-3.5", running && "animate-pulse")} />
          </Button>

          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" className="h-7 w-7">
                <MoreHorizontal className="h-3.5 w-3.5" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={handleToggleState}>
                {automation.state === "active" ? (
                  <>
                    <Pause className="mr-2 h-3.5 w-3.5" />
                    Pause
                  </>
                ) : (
                  <>
                    <Play className="mr-2 h-3.5 w-3.5" />
                    Resume
                  </>
                )}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-red-400 focus:text-red-400"
                onClick={() => setDeleteOpen(true)}
              >
                <Trash2 className="mr-2 h-3.5 w-3.5" />
                Delete
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      {/* Delete confirmation dialog */}
      <Dialog open={deleteOpen} onOpenChange={setDeleteOpen}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>Delete automation</DialogTitle>
            <DialogDescription>
              Delete "{automation.name}"? This action cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteOpen(false)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleting}
            >
              {deleting ? "Deleting..." : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

// ---------------------------------------------------------------------------
// Empty state
// ---------------------------------------------------------------------------

function EmptyState() {
  return (
    <div className="flex flex-col items-center justify-center py-20 px-4 text-center">
      <div className="flex h-12 w-12 items-center justify-center rounded-full bg-accent mb-4">
        <Workflow className="h-6 w-6 text-muted-foreground" />
      </div>
      <h3 className="text-sm font-medium mb-1">No automations</h3>
      <p className="text-sm text-muted-foreground max-w-sm">
        Automations let you run workflows on a schedule, in response to events, or manually.
        Create your first automation to get started.
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Automations Page
// ---------------------------------------------------------------------------

export function AutomationsPage() {
  const { filteredAutomations, selectedProject, snapshot, refreshAutomations } = useAppState();
  const projects = snapshot?.projects ?? [];

  // Map project IDs to repo names
  const projectIdToRepo: Record<string, string> = {};
  for (const p of projects) {
    projectIdToRepo[p.id] = p.repo;
  }

  const projectName = selectedProject
    ? projectIdToRepo[selectedProject] ?? "Project"
    : "All Projects";

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold">
            {selectedProject ? projectIdToRepo[selectedProject] ?? "Automations" : "Automations"}
          </h1>
          <span className="text-xs text-muted-foreground">
            {filteredAutomations.length} {filteredAutomations.length === 1 ? "automation" : "automations"}
          </span>
        </div>

        <Button size="sm" className="h-7 text-xs gap-1" disabled>
          <Plus className="h-3.5 w-3.5" />
          New Automation
        </Button>
      </div>

      {/* List */}
      <ScrollArea className="flex-1">
        {filteredAutomations.length === 0 ? (
          <EmptyState />
        ) : (
          <div>
            {filteredAutomations.map((automation) => (
              <AutomationRow
                key={automation.id}
                automation={automation}
                projectName={projectIdToRepo[automation.project_id] ?? automation.project_id}
                onRefresh={refreshAutomations}
              />
            ))}
          </div>
        )}
      </ScrollArea>
    </div>
  );
}
