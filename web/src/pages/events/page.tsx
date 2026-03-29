import { useMemo, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn, formatRelativeTime } from "@/lib/utils";
import { ListView, ListEmptyState } from "@/components/ui/list-view";
import { ListHeader, ListHeaderTabs } from "@/components/ui/list-header";
import {
  ListRow,
  TimeCell,
  BadgeCell,
  TextCell,
  IdCell,
} from "@/components/ui/list-row";
import type { Event } from "@/lib/types";

// ---------------------------------------------------------------------------
// Filter categories
// ---------------------------------------------------------------------------

const FILTERS = [
  { key: "important", label: "Important" },
  { key: "all", label: "All" },
  { key: "task", label: "Task" },
  { key: "agent", label: "Agent" },
  { key: "merge", label: "Merge" },
  { key: "github", label: "GitHub" },
  { key: "system", label: "System" },
  { key: "orchestrator", label: "Orchestrator" },
  { key: "automation", label: "Automation" },
] as const;

type FilterKey = (typeof FILTERS)[number]["key"];

/** Event types excluded from "Important" filter (high-frequency, verbose events) */
const VERBOSE_EVENT_TYPES = ["agent:message", "human:message"];

function badgeClasses(type: string): string {
  if (type.startsWith("task:")) return "bg-blue-500/15 text-blue-400 border-blue-500/30";
  if (type.startsWith("agent:")) return "bg-green-500/15 text-green-400 border-green-500/30";
  if (type.startsWith("merge:")) return "bg-purple-500/15 text-purple-400 border-purple-500/30";
  if (type.startsWith("system:")) return "bg-gray-500/15 text-gray-400 border-gray-500/30";
  if (type.startsWith("orchestrator:")) return "bg-orange-500/15 text-orange-400 border-orange-500/30";
  if (type.startsWith("automation:")) return "bg-teal-500/15 text-teal-400 border-teal-500/30";
  if (type.startsWith("github:")) return "bg-sky-500/15 text-sky-400 border-sky-500/30";
  return "bg-muted text-muted-foreground";
}

function matchesFilter(event: Event, filter: FilterKey): boolean {
  if (filter === "all") return true;
  if (filter === "important") return !VERBOSE_EVENT_TYPES.includes(event.type);
  return event.type.startsWith(`${filter}:`);
}

function truncateData(data: Record<string, unknown>, max = 100): string {
  const raw = JSON.stringify(data);
  return raw.length > max ? `${raw.slice(0, max)}...` : raw;
}

// ---------------------------------------------------------------------------
// Event row component
// ---------------------------------------------------------------------------

function EventRow({ event }: { event: Event }) {
  return (
    <ListRow clickable={false}>
      <TimeCell width="w-20">{formatRelativeTime(event.ts)}</TimeCell>
      <BadgeCell>
        <Badge variant="outline" className={cn("font-mono text-xs", badgeClasses(event.type))}>
          {event.type}
        </Badge>
      </BadgeCell>
      <TextCell flex={false} size="xs" className="w-28">{event.actor}</TextCell>
      <IdCell width="w-20">{event.task ? event.task.slice(0, 8) : "\u2014"}</IdCell>
      <TextCell size="xs" muted mono className="max-w-[400px]">
        {truncateData(event.data)}
      </TextCell>
    </ListRow>
  );
}

// ---------------------------------------------------------------------------
// Events page
// ---------------------------------------------------------------------------

export function EventsPage() {
  const { events: liveEvents } = useAppState();
  const [activeFilter, setActiveFilter] = useState<FilterKey>("important");
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

  const tabsConfig = FILTERS.map(({ key, label }) => ({
    key,
    label,
  }));

  const headerTabs = (
    <ListHeaderTabs
      tabs={tabsConfig}
      activeTab={activeFilter}
      onTabChange={(tab) => setActiveFilter(tab as FilterKey)}
    />
  );

  const headerActions = (
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
  );

  return (
    <ListView
      header={
        <ListHeader
          title="Events"
          tabs={headerTabs}
          actions={headerActions}
        />
      }
      isEmpty={filteredEvents.length === 0}
      emptyState={
        <ListEmptyState
          message={paused ? "No events match the current filter (paused)." : "No events yet."}
        />
      }
    >
      <div>
        {filteredEvents.map((event) => (
          <EventRow key={event.id} event={event} />
        ))}
      </div>
    </ListView>
  );
}

export default EventsPage;
