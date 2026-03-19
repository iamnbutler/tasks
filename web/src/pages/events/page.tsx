import { useMemo, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ScrollArea } from "@/components/ui/scroll-area";
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

function badgeClasses(type: string): string {
  if (type.startsWith("task:")) return "bg-blue-500/15 text-blue-400 border-blue-500/30";
  if (type.startsWith("agent:")) return "bg-green-500/15 text-green-400 border-green-500/30";
  if (type.startsWith("merge:")) return "bg-purple-500/15 text-purple-400 border-purple-500/30";
  if (type.startsWith("system:")) return "bg-gray-500/15 text-gray-400 border-gray-500/30";
  if (type.startsWith("orchestrator:")) return "bg-orange-500/15 text-orange-400 border-orange-500/30";
  return "bg-muted text-muted-foreground";
}

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

  const frozenRef = useRef<Event[]>([]);

  function handleTogglePause() {
    if (!paused) {
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
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-4">
          <h1 className="text-sm font-semibold">Events</h1>

          <Tabs
            value={activeFilter}
            onValueChange={(v: string) => setActiveFilter(v as FilterKey)}
          >
            <TabsList className="h-7">
              {FILTERS.map(({ key, label }) => (
                <TabsTrigger key={key} value={key} className="text-xs px-2.5 h-6">
                  {label}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </div>

        <Button
          size="sm"
          variant="outline"
          onClick={handleTogglePause}
          className="gap-1.5 h-7 text-xs"
        >
          {paused ? (
            <>
              <Play className="h-3 w-3" />
              Resume
            </>
          ) : (
            <>
              <Pause className="h-3 w-3" />
              Pause
            </>
          )}
        </Button>
      </div>

      {/* Events table */}
      <ScrollArea className="flex-1">
        {filteredEvents.length === 0 ? (
          <div className="flex items-center justify-center py-20">
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
                  <TableCell className="text-xs text-muted-foreground whitespace-nowrap">
                    {formatRelativeTime(event.ts)}
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline" className={cn("font-mono text-xs", badgeClasses(event.type))}>
                      {event.type}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-xs">{event.actor}</TableCell>
                  <TableCell className="font-mono text-xs text-muted-foreground">
                    {event.task ? event.task.slice(0, 8) : "\u2014"}
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground max-w-[400px] truncate font-mono">
                    {truncateData(event.data)}
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

export default EventsPage;
