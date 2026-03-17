import { useMemo, useState } from "react";
import {
  useReactTable,
  getCoreRowModel,
  getPaginationRowModel,
  flexRender,
} from "@tanstack/react-table";
import { useAppState } from "@/hooks/use-app-state";
import { flushMergeQueue } from "@/lib/api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
import { columns } from "./columns";
import type { Mode } from "@/lib/types";

// ---------------------------------------------------------------------------
// Mode badge
// ---------------------------------------------------------------------------

function modeBadge(mode: Mode) {
  switch (mode) {
    case "play":
      return <Badge className="bg-green-600 text-white">Play</Badge>;
    case "pause":
      return <Badge className="bg-yellow-600 text-white">Pause</Badge>;
    case "stop":
      return <Badge className="bg-red-600 text-white">Stop</Badge>;
    default:
      return <Badge variant="outline">{mode}</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Merge Queue Page
// ---------------------------------------------------------------------------

export function MergeQueuePage() {
  const { snapshot, refreshSnapshot, filteredMergeQueue, selectedProject } = useAppState();
  const [flushing, setFlushing] = useState(false);

  const entries = filteredMergeQueue;

  const approvedCount = useMemo(
    () => entries.filter((e) => e.status === "approved").length,
    [entries],
  );

  const mode = snapshot?.mode;
  const isPaused = mode === "pause";

  // Find the selected project name for display
  const selectedProjectName = selectedProject
    ? snapshot?.projects.find((p) => p.id === selectedProject)?.repo
    : null;

  const table = useReactTable({
    data: entries,
    columns,
    getCoreRowModel: getCoreRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    state: {
      // Hide project column when a specific project is selected
      columnVisibility: selectedProject ? { project: false } : {},
    },
    meta: {
      refreshSnapshot,
      tasks: snapshot?.tasks ?? [],
      projects: snapshot?.projects ?? [],
    },
  });

  async function handleFlush() {
    setFlushing(true);
    try {
      await flushMergeQueue();
      await refreshSnapshot();
    } finally {
      setFlushing(false);
    }
  }

  // -------------------------------------------------------------------------
  // Loading state
  // -------------------------------------------------------------------------

  if (!snapshot) {
    return (
      <div className="flex items-center justify-center h-full py-32">
        <p className="text-muted-foreground text-sm">Loading...</p>
      </div>
    );
  }

  // -------------------------------------------------------------------------
  // Render
  // -------------------------------------------------------------------------

  return (
    <div className="space-y-6 p-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-3">
            <h1 className="text-base font-bold">Merge Queue</h1>
            {mode && modeBadge(mode)}
          </div>
          {selectedProjectName && (
            <p className="text-sm text-muted-foreground">
              Showing entries for {selectedProjectName}
            </p>
          )}
        </div>
        {isPaused && (
          <div className="flex items-center gap-3">
            {approvedCount > 0 && (
              <span className="text-sm text-muted-foreground">
                {approvedCount} approved {approvedCount === 1 ? "entry" : "entries"} ready to flush
              </span>
            )}
            <Button
              onClick={handleFlush}
              disabled={flushing || approvedCount === 0}
            >
              {flushing ? "Flushing..." : "Flush Queue"}
            </Button>
          </div>
        )}
      </div>

      {/* Table */}
      <Card>
        <CardHeader>
          <CardTitle className="text-sm font-medium text-muted-foreground">
            {entries.length} {entries.length === 1 ? "entry" : "entries"}
          </CardTitle>
        </CardHeader>
        <CardContent>
          {entries.length === 0 ? (
            <p className="text-sm text-muted-foreground py-8 text-center">
              No entries in the merge queue.
            </p>
          ) : (
            <>
              <div className="overflow-x-auto">
                <Table>
                  <TableHeader>
                    {table.getHeaderGroups().map((headerGroup) => (
                      <TableRow key={headerGroup.id}>
                        {headerGroup.headers.map((header) => (
                          <TableHead key={header.id}>
                            {header.isPlaceholder
                              ? null
                              : flexRender(
                                  header.column.columnDef.header,
                                  header.getContext(),
                                )}
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
                            {flexRender(
                              cell.column.columnDef.cell,
                              cell.getContext(),
                            )}
                          </TableCell>
                        ))}
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>

              {/* Pagination */}
              {table.getPageCount() > 1 && (
                <div className="flex items-center justify-between pt-4">
                  <span className="text-sm text-muted-foreground">
                    Page {table.getState().pagination.pageIndex + 1} of{" "}
                    {table.getPageCount()}
                  </span>
                  <div className="flex items-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => table.previousPage()}
                      disabled={!table.getCanPreviousPage()}
                    >
                      Previous
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => table.nextPage()}
                      disabled={!table.getCanNextPage()}
                    >
                      Next
                    </Button>
                  </div>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
