import { useMemo, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn, formatRelativeTime } from "@/lib/utils";
import type { Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// Filter categories
// ---------------------------------------------------------------------------

const FILTERS = [
  { key: "all", label: "All" },
  { key: "task", label: "Task" },
  { key: "agent", label: "Agent" },
  { key: "merge", label: "Merge" },
  { key: "system", label: "System" },
  { key: "orchestrator", label: "Orchestrator" },
] as const;

type FilterKey = (typeof FILTERS)[number]["key"];

// ---------------------------------------------------------------------------
// Event type badge colors
// ---------------------------------------------------------------------------

function badgeClasses(type: string): string {
  if (type.startsWith("task:")) return "bg-blue-500/15 text-blue-600 border-blue-500/30";
  if (type.startsWith("agent:")) return "bg-green-500/15 text-green-600 border-green-500/30";
  if (type.startsWith("merge:")) return "bg-purple-500/15 text-purple-600 border-purple-500/30";
  if (type.startsWith("system:")) return "bg-gray-500/15 text-gray-600 border-gray-500/30";
  if (type.startsWith("orchestrator:")) return "bg-orange-500/15 text-orange-600 border-orange-500/30";
  return "bg-muted text-muted-foreground";
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function matchesFilter(event: Event, filter: FilterKey): boolean {
  if (filter === "all") return true;
  return event.type.startsWith(`${filter}:`);
}

function truncateData(data: Record<string, unknown>, max = 100): string {
  const raw = JSON.stringify(data);
  return raw.length > max ? `${raw.slice(0, max)}...` : raw;
}

// ---------------------------------------------------------------------------
// Events page
// ---------------------------------------------------------------------------

export function EventsPage() {
  const { events: liveEvents } = useAppState();
  const [activeFilter, setActiveFilter] = useState<FilterKey>("all");
  const [paused, setPaused] = useState(false);

  // When the user pauses, freeze the current event list into the ref.
  const frozenRef = useRef<Event[]>([]);

  function handleTogglePause() {
    if (!paused) {
      // Freezing: capture current live events.
      frozenRef.current = liveEvents;
    }
    setPaused((prev) => !prev);
  }

  const events = paused ? frozenRef.current : liveEvents;

  const filteredEvents = useMemo(
    () => events.filter((e) => matchesFilter(e, activeFilter)),
    [events, activeFilter],
  );

  return (
    <div className="flex flex-col h-full">
      {/* Toolbar */}
      <div className="flex items-center justify-between gap-4 border-b border-border px-6 py-3">
        {/* Filter buttons */}
        <div className="flex items-center gap-1.5 flex-wrap">
          {FILTERS.map(({ key, label }) => (
            <Button
              key={key}
              size="sm"
              variant={activeFilter === key ? "default" : "outline"}
              onClick={() => setActiveFilter(key)}
              className="h-7"
            >
              {label}
            </Button>
          ))}
        </div>

        {/* Pause / Resume */}
        <Button
          size="sm"
          variant="outline"
          onClick={handleTogglePause}
          className="gap-1.5 shrink-0"
        >
          {paused ? (
            <>
              <Play className="h-3.5 w-3.5" />
              Resume
            </>
          ) : (
            <>
              <Pause className="h-3.5 w-3.5" />
              Pause
            </>
          )}
        </Button>
      </div>

      {/* Events table */}
      <div className="flex-1 overflow-auto">
        {filteredEvents.length === 0 ? (
          <div className="flex items-center justify-center h-full py-32">
            <p className="text-muted-foreground text-sm">
              {paused ? "No events match the current filter (paused)." : "No events yet."}
            </p>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[100px]">Time</TableHead>
                <TableHead className="w-[180px]">Type</TableHead>
                <TableHead className="w-[110px]">Actor</TableHead>
                <TableHead className="w-[90px]">Task</TableHead>
                <TableHead>Data</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filteredEvents.map((event) => (
                <TableRow key={event.id}>
                  <TableCell className="text-sm text-muted-foreground whitespace-nowrap">
                    {formatRelativeTime(event.ts)}
                  </TableCell>
                  <TableCell>
                    <Badge
                      variant="outline"
                      className={cn("font-mono", badgeClasses(event.type))}
                    >
                      {event.type}
                    </Badge>
                  </TableCell>
                  <TableCell>{event.actor}</TableCell>
                  <TableCell className="font-mono text-sm text-muted-foreground">
                    {event.task ? event.task.slice(0, 8) : "\u2014"}
                  </TableCell>
                  <TableCell className="text-sm text-muted-foreground max-w-[400px] truncate font-mono">
                    {truncateData(event.data)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </div>
    </div>
  );
}

export default EventsPage;
