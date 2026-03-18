import { useMemo } from "react";
import { useAppState } from "@/hooks/use-app-state";
import { columns } from "./columns";
import { DataTable } from "./data-table";

export function TasksPage() {
  const { filteredTasks, selectedProject, snapshot } = useAppState();
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
    <div className="flex flex-1 flex-col p-4">
      <DataTable columns={columns} data={filteredTasks} hideProjectColumn={!!selectedProject} projectIdToRepo={projectIdToRepo} />
    </div>
  );
}
