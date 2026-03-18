import { useMemo, useState } from "react";
import {
  type PaginationState,
  useReactTable,
  getCoreRowModel,
  getPaginationRowModel,
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
import { columns } from "./columns";

export function MergeQueuePage() {
  const { snapshot, refreshSnapshot, filteredMergeQueue, selectedProject } = useAppState();
  const [flushing, setFlushing] = useState(false);
  const [pagination, setPagination] = useState<PaginationState>({
    pageIndex: 0,
    pageSize: 50,
  });

  const entries = filteredMergeQueue;

  const approvedCount = useMemo(
    () => entries.filter((e) => e.status === "approved").length,
    [entries],
  );

  const isPaused = snapshot?.mode === "pause";

  const table = useReactTable({
    data: entries,
    columns,
    getCoreRowModel: getCoreRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    state: {
      pagination,
      columnVisibility: selectedProject ? { project: false } : {},
    },
    autoResetPageIndex: false,
    onPaginationChange: setPagination,
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

      {/* Table */}
      <ScrollArea className="flex-1">
        {entries.length === 0 ? (
          <div className="flex items-center justify-center py-20">
            <p className="text-sm text-muted-foreground">No entries in the merge queue.</p>
          </div>
        ) : (
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
        )}
      </ScrollArea>
    </div>
  );
}
