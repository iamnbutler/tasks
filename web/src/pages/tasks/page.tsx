import { useMemo } from "react";
import { useAppState } from "@/hooks/use-app-state";
import { columns } from "./columns";
import { DataTable } from "./data-table";

export function TasksPage() {
  const { filteredTasks, selectedProject, snapshot } = useAppState();
  const projects = snapshot?.projects ?? [];

  // Find the selected project name for display
  const selectedProjectName = selectedProject
    ? snapshot?.projects.find((p) => p.id === selectedProject)?.repo
    : null;

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
          {selectedProjectName
            ? `Tasks for ${selectedProjectName}`
            : "Manage and monitor your coding tasks."}
        </p>
      </div>
      <DataTable columns={columns} data={filteredTasks} hideProjectColumn={!!selectedProject} projectIdToRepo={projectIdToRepo} />
    </div>
  );
}
