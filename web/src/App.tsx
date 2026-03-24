import { Routes, Route } from "react-router-dom";
import { AppStateProvider } from "@/hooks/use-app-state";
import { Layout } from "@/components/layout";
import { DashboardPage } from "@/pages/dashboard/page";
import { TasksPage } from "@/pages/tasks/page";
import { TaskDetailPage } from "@/pages/task-detail";
import { MergeQueuePage } from "@/pages/merge-queue/page";
import { ContainersPage } from "@/pages/containers/page";
import { AutomationsPage } from "@/pages/automations/page";
import { AutomationDetailPage } from "@/pages/automation-detail";
import { OrchestratorPage } from "@/pages/orchestrator/page";
import { EventsPage } from "@/pages/events/page";

export function App() {
  return (
    <AppStateProvider>
      <Routes>
        <Route element={<Layout />}>
          <Route path="/" element={<DashboardPage />} />
          <Route path="/tasks" element={<TasksPage />} />
          <Route path="/tasks/:id" element={<TaskDetailPage />} />
          <Route path="/merge-queue" element={<MergeQueuePage />} />
          <Route path="/containers" element={<ContainersPage />} />
          <Route path="/automations" element={<AutomationsPage />} />
          <Route path="/automations/:id" element={<AutomationDetailPage />} />
          <Route path="/orchestrator" element={<OrchestratorPage />} />
          <Route path="/events" element={<EventsPage />} />
        </Route>
      </Routes>
    </AppStateProvider>
  );
}
