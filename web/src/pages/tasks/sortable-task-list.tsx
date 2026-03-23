import { useState, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import {
  DndContext,
  closestCenter,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  DragEndEvent,
} from "@dnd-kit/core";
import {
  arrayMove,
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import {
  ArrowDown,
  ArrowRight,
  ArrowUp,
  GripVertical,
  Minus,
} from "lucide-react";
import { cn, formatRelativeTime } from "@/lib/utils";
import { reorderTasks } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import type { Task } from "@/lib/types";
import { taskStateMeta } from "./columns";

// ---------------------------------------------------------------------------
// Priority indicator
// ---------------------------------------------------------------------------

const priorityConfig: Record<
  number,
  { icon: typeof ArrowUp; className: string }
> = {
  1: { icon: ArrowUp, className: "text-red-500" },
  2: { icon: ArrowRight, className: "text-yellow-500" },
  3: { icon: ArrowDown, className: "text-blue-500" },
};

function PriorityIcon({ priority }: { priority: number | null }) {
  if (priority == null) {
    return <Minus className="h-3.5 w-3.5 text-muted-foreground/50" />;
  }
  const config = priorityConfig[priority];
  if (!config) {
    return (
      <span className="text-xs font-mono text-muted-foreground w-3.5 text-center">
        {priority}
      </span>
    );
  }
  const Icon = config.icon;
  return <Icon className={cn("h-3.5 w-3.5", config.className)} />;
}

// ---------------------------------------------------------------------------
// Sortable Task Item
// ---------------------------------------------------------------------------

interface SortableTaskItemProps {
  task: Task;
  projectName: string;
}

function SortableTaskItem({ task, projectName }: SortableTaskItemProps) {
  const navigate = useNavigate();
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: task.id });

  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
  };

  const meta = taskStateMeta[task.state];
  const StateIcon = meta?.icon;

  const idLabel =
    task.source.type === "github_issue" || task.source.type === "github_pr"
      ? `#${task.source.number}`
      : task.id.slice(0, 8);

  return (
    <div
      ref={setNodeRef}
      style={style}
      className={cn(
        "flex w-full items-center gap-3 px-4 py-2 bg-background border-b border-border transition-colors",
        isDragging && "opacity-50 bg-accent/50 shadow-lg z-50"
      )}
    >
      {/* Drag handle */}
      <button
        className="cursor-grab touch-none p-1 -ml-1 text-muted-foreground hover:text-foreground"
        {...attributes}
        {...listeners}
      >
        <GripVertical className="h-4 w-4" />
      </button>

      {/* Task row content (clickable to navigate) */}
      <button
        onClick={() => navigate(`/tasks/${task.id}`)}
        className="flex flex-1 items-center gap-3 text-left hover:bg-accent/30 transition-colors rounded px-2 py-1 -mx-2 -my-1"
      >
        {/* Priority */}
        <PriorityIcon priority={task.priority} />

        {/* ID */}
        <span className="w-16 shrink-0 font-mono text-xs text-muted-foreground">
          {idLabel}
        </span>

        {/* State icon */}
        {StateIcon && (
          <StateIcon className={cn("h-4 w-4 shrink-0", meta.color)} />
        )}

        {/* Title */}
        <span className="flex-1 truncate text-sm">{task.title}</span>

        {/* Labels */}
        {task.labels.map((label) => (
          <Badge key={label} variant="outline" className="text-xs shrink-0">
            {label}
          </Badge>
        ))}

        {/* Project */}
        <span className="shrink-0 text-xs text-muted-foreground">
          {projectName}
        </span>

        {/* Updated */}
        <span className="w-16 shrink-0 text-right text-xs text-muted-foreground">
          {formatRelativeTime(task.updated_at)}
        </span>
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sortable Task List
// ---------------------------------------------------------------------------

interface SortableTaskListProps {
  tasks: Task[];
  projectIdToRepo: Record<string, string>;
  onReorder: (taskIds: string[]) => void;
}

export function SortableTaskList({
  tasks,
  projectIdToRepo,
  onReorder,
}: SortableTaskListProps) {
  const [items, setItems] = useState(tasks);
  const [isReordering, setIsReordering] = useState(false);

  // Update items when tasks prop changes (but not during drag)
  // This is a simplified approach - in production you'd want to be smarter about reconciliation
  if (!isReordering && tasks.length !== items.length) {
    setItems(tasks);
  }

  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: {
        distance: 8,
      },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    })
  );

  const handleDragStart = useCallback(() => {
    setIsReordering(true);
  }, []);

  const handleDragEnd = useCallback(
    async (event: DragEndEvent) => {
      const { active, over } = event;

      if (over && active.id !== over.id) {
        const oldIndex = items.findIndex((item) => item.id === active.id);
        const newIndex = items.findIndex((item) => item.id === over.id);

        const newItems = arrayMove(items, oldIndex, newIndex);
        setItems(newItems);

        // Send to server
        const taskIds = newItems.map((item) => item.id);
        try {
          await reorderTasks(taskIds);
          onReorder(taskIds);
        } catch (error) {
          console.error("Failed to reorder tasks:", error);
          // Revert on error
          setItems(items);
        }
      }

      setIsReordering(false);
    },
    [items, onReorder]
  );

  const handleDragCancel = useCallback(() => {
    setIsReordering(false);
  }, []);

  if (items.length === 0) {
    return (
      <div className="flex items-center justify-center py-20">
        <p className="text-sm text-muted-foreground">
          No tasks in queue.
        </p>
      </div>
    );
  }

  return (
    <DndContext
      sensors={sensors}
      collisionDetection={closestCenter}
      onDragStart={handleDragStart}
      onDragEnd={handleDragEnd}
      onDragCancel={handleDragCancel}
    >
      <SortableContext items={items} strategy={verticalListSortingStrategy}>
        <div>
          {items.map((task) => (
            <SortableTaskItem
              key={task.id}
              task={task}
              projectName={projectIdToRepo[task.project] ?? task.project}
            />
          ))}
        </div>
      </SortableContext>
    </DndContext>
  );
}
