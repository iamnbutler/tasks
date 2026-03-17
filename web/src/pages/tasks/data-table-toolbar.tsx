import type { Table } from "@tanstack/react-table";
import { X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { Task } from "@/lib/types";
import { DataTableFacetedFilter } from "./data-table-faceted-filter";
import { taskStateMeta } from "./columns";

// Build state filter options from the shared metadata.
const stateOptions = Object.entries(taskStateMeta).map(([value, meta]) => ({
  value,
  label: meta.label,
  icon: meta.icon,
}));

interface DataTableToolbarProps {
  table: Table<Task>;
  projectIdToRepo: Record<string, string>;
}

export function DataTableToolbar({ table, projectIdToRepo }: DataTableToolbarProps) {
  const isFiltered = table.getState().columnFilters.length > 0;

  // Derive unique project IDs from table data, display repo names.
  const projectOptions = Array.from(
    new Set(table.getCoreRowModel().rows.map((row) => row.original.project))
  )
    .sort((a, b) => {
      const repoA = projectIdToRepo[a] ?? a;
      const repoB = projectIdToRepo[b] ?? b;
      return repoA.localeCompare(repoB);
    })
    .map((projectId) => ({
      value: projectId,
      label: projectIdToRepo[projectId] ?? projectId,
    }));

  return (
    <div className="flex items-center justify-between">
      <div className="flex flex-1 items-center space-x-2">
        <Input
          placeholder="Filter tasks..."
          value={
            (table.getColumn("title")?.getFilterValue() as string) ?? ""
          }
          onChange={(event) =>
            table.getColumn("title")?.setFilterValue(event.target.value)
          }
          className="h-8 w-[150px] lg:w-[250px]"
        />
        {table.getColumn("state") && (
          <DataTableFacetedFilter
            column={table.getColumn("state")}
            title="State"
            options={stateOptions}
          />
        )}
        {/* Only show project filter when viewing all projects (column visible) */}
        {table.getColumn("project")?.getIsVisible() && projectOptions.length > 0 && (
          <DataTableFacetedFilter
            column={table.getColumn("project")}
            title="Project"
            options={projectOptions}
          />
        )}
        {isFiltered && (
          <Button
            variant="ghost"
            onClick={() => table.resetColumnFilters()}
            className="h-8 px-2 lg:px-3"
          >
            Reset
            <X className="ml-2 h-4 w-4" />
          </Button>
        )}
      </div>
    </div>
  );
}ton>
        )}
      </div>
    </div>
  );
}
