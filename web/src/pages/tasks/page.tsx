import { useAppState } from "@/hooks/use-app-state";
import { columns } from "./columns";
import { DataTable } from "./data-table";

export function TasksPage() {
  const { snapshot } = useAppState();
  const tasks = snapshot?.tasks ?? [];

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 md:p-8">
      <div>
        <h2 className="text-2xl font-bold tracking-tight">Tasks</h2>
        <p className="text-muted-foreground">
          Manage and monitor your coding tasks.
        </p>
      </div>
      <DataTable columns={columns} data={tasks} />
    </div>
  );
}
