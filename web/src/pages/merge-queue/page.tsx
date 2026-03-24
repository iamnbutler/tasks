import { useMemo, useState } from "react";
import {
  type PaginationState,
  type SortingState,
  useReactTable,
  getCoreRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  flexRender,
} from "@tanstack/react-table";
import { useAppState } from "@/hooks/use-app-state";
import { flushMergeQueue } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { columns, lifecyclePhases, type LifecyclePhase } from "./columns";
import type { MergeQueueEntry } from "@/lib/types";

function MergeTable({
  entries,
  selectedProject,
  refreshSnapshot,
  snapshot,
}: {
  entries: MergeQueueEntry[];
  selectedProject: string | null;
  refreshSnapshot: () => Promise<void>;
  snapshot: NonNullable<ReturnType<typeof useAppState>["snapshot"]>;
}) {
  const [pagination, setPagination] = useState<PaginationState>({
    pageIndex: 0,
    pageSize: 50,
  });
  const [sorting] = useState<SortingState>([{ id: "status", desc: false }]);

  const table = useReactTable({
    data: entries,
    columns,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    state: {
      sorting,
      pagination,
      columnVisibility: selectedProject ? { project: false } : {},
    },
    autoResetPageIndex: false,
    onPaginationChange: setPagination,
    meta: {
      refreshSnapshot,
      tasks: snapshot.tasks ?? [],
      projects: snapshot.projects ?? [],
    },
  });

  if (entries.length === 0) {
    return (
      <div className="flex items-center justify-center py-20">
        <p className="text-sm text-muted-foreground">No entries in this phase.</p>
      </div>
    );
  }

  return (
    <>
      <Table>
        <TableHeader>
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id}>
              {headerGroup.headers.map((header) => (
                <TableHead key={header.id}>
                  {header.isPlaceholder
                    ? null
                    : flexRender(header.column.columnDef.header, header.getContext())}
                </TableHead>
              ))}
            </TableRow>
          ))}
        </TableHeader>
        <TableBody>
          {table.getRowModel().rows.map((row) => (
            <TableRow key={row.id}>
              {row.getVisibleCells().map((cell) => (
                <TableCell key={cell.id}>
                  {flexRender(cell.column.columnDef.cell, cell.getContext())}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>

      {table.getPageCount() > 1 && (
        <div className="flex items-center justify-between px-4 py-2 border-t">
          <span className="text-xs text-muted-foreground">
            Page {table.getState().pagination.pageIndex + 1} of {table.getPageCount()}
          </span>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-xs"
              onClick={() => table.previousPage()}
              disabled={!table.getCanPreviousPage()}
            >
              Previous
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-xs"
              onClick={() => table.nextPage()}
              disabled={!table.getCanNextPage()}
            >
              Next
            </Button>
          </div>
        </div>
      )}
    </>
  );
}

export function MergeQueuePage() {
  const { snapshot, refreshSnapshot, filteredMergeQueue, selectedProject } = useAppState();
  const [flushing, setFlushing] = useState(false);
  const [activeTab, setActiveTab] = useState<LifecyclePhase>("review");

  const entries = filteredMergeQueue;

  // Group entries by lifecycle phase
  const groupedEntries = useMemo(() => {
    const groups: Record<LifecyclePhase, MergeQueueEntry[]> = {
      review: [],
      queue: [],
      completed: [],
    };

    for (const entry of entries) {
      const statuses = lifecyclePhases.review.statuses;
      if (statuses.includes(entry.status)) {
        groups.review.push(entry);
      } else if (lifecyclePhases.queue.statuses.includes(entry.status)) {
        groups.queue.push(entry);
      } else if (lifecyclePhases.completed.statuses.includes(entry.status)) {
        groups.completed.push(entry);
      }
    }

    return groups;
  }, [entries]);

  const approvedCount = groupedEntries.queue.length;
  const isPaused = snapshot?.mode === "pause";

  async function handleFlush() {
    setFlushing(true);
    try {
      await flushMergeQueue();
      await refreshSnapshot();
    } finally {
      setFlushing(false);
    }
  }

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-2.5">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold">Merge Queue</h1>
          <span className="text-xs text-muted-foreground">{entries.length} entries</span>
        </div>

        {isPaused && approvedCount > 0 && (
          <Button
            size="sm"
            onClick={handleFlush}
            disabled={flushing}
            className="h-7 text-xs"
          >
            {flushing ? "Flushing..." : `Flush ${approvedCount} approved`}
          </Button>
        )}
      </div>

      {/* Tabs */}
      <Tabs
        value={activeTab}
        onValueChange={(v) => setActiveTab(v as LifecyclePhase)}
        className="flex flex-col flex-1 min-h-0"
      >
        <div className="px-4 pt-3 pb-1 border-b border-border">
          <TabsList variant="line">
            {(Object.entries(lifecyclePhases) as [LifecyclePhase, typeof lifecyclePhases.review][]).map(
              ([phase, config]) => (
                <TabsTrigger key={phase} value={phase}>
                  {config.label} ({groupedEntries[phase].length})
                </TabsTrigger>
              )
            )}
          </TabsList>
        </div>

        <ScrollArea className="flex-1">
          {(Object.keys(lifecyclePhases) as LifecyclePhase[]).map((phase) => (
            <TabsContent key={phase} value={phase} className="m-0">
              <MergeTable
                entries={groupedEntries[phase]}
                selectedProject={selectedProject}
                refreshSnapshot={refreshSnapshot}
                snapshot={snapshot}
              />
            </TabsContent>
          ))}
        </ScrollArea>
      </Tabs>
    </div>
  );
}
