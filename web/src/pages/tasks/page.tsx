import { useMemo } from "react";
import { useAppState } from "@/hooks/use-app-state";
import { columns } from "./columns";
import { DataTable } from "./data-table";

export function TasksPage() {
  const { snapshot } = useAppState();
  const tasks = snapshot?.tasks ?? [];
  const projects = snapshot?.projects ?? [];

  // Create a map from project ID to repo name for display
  const projectIdToRepo = useMemo(() => {
    const map: Record<string, string> = {};
    for (const p of projects) {
      map[p.id] = p.repo;
    }
    return map;
  }, [projects]);

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 md:p-8">
      <div>
        <h2 className="text-2xl font-bold tracking-tight">Tasks</h2>
        <p className="text-muted-foreground">
          Manage and monitor your coding tasks.
        </p>
      </div>
      <DataTable columns={columns} data={tasks} projectIdToRepo={projectIdToRepo} />
    </div>
  );
}
