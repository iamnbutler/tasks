import { useAppState } from "@/hooks/use-app-state";
import { columns } from "./columns";
import { DataTable } from "./data-table";

export function TasksPage() {
  const { filteredTasks, selectedProject, snapshot } = useAppState();

  // Find the selected project name for display
  const selectedProjectName = selectedProject
    ? snapshot?.projects.find((p) => p.id === selectedProject)?.repo
    : null;

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
      <DataTable columns={columns} data={filteredTasks} hideProjectColumn={!!selectedProject} />
    </div>
  );
}
