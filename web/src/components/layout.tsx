import { NavLink, Outlet } from "react-router-dom";
import {
  LayoutDashboard,
  ListTodo,
  GitMerge,
  Radio,
  MonitorDot,
  Clock,
  CircleHelp,
  Layers,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { ModeControl } from "@/components/mode-control";
import { cn } from "@/lib/utils";

const navItems = [
  { to: "/", label: "Dashboard", icon: LayoutDashboard },
  { to: "/tasks", label: "Tasks", icon: ListTodo },
  { to: "/merge-queue", label: "Merge Queue", icon: GitMerge },
  { to: "/events", label: "Events", icon: Radio },
];

export function Layout() {
  const { snapshot, connected } = useAppState();

  const tasks = snapshot?.tasks ?? [];
  const runningCount = tasks.filter((t) => t.state === "running").length;
  const waitingCount = tasks.filter((t) => t.state === "waiting").length;
  const questionCount = tasks.filter((t) => t.state === "question").length;
  const mergeQueueCount = snapshot?.merge_queue?.length ?? 0;
  const slotActive = snapshot?.slot_utilization?.active ?? 0;
  const slotMax = snapshot?.slot_utilization?.max ?? 0;

  return (
    <div className="flex h-screen bg-background text-foreground">
      {/* Sidebar */}
      <aside className="flex w-60 flex-col border-r border-border bg-background">
        {/* App title */}
        <div className="border-b border-border px-4 py-4">
          <h1 className="text-lg font-bold tracking-tight">Tasks</h1>
        </div>

        {/* Mode control */}
        <div className="border-b border-border px-4 py-3">
          <ModeControl />
        </div>

        {/* Status section */}
        <div className="border-b border-border px-4 py-3 space-y-1.5 text-sm">
          <StatusRow icon={MonitorDot} label="Sessions" value={`${slotActive}/${slotMax}`} />
          <StatusRow icon={Layers} label="Running" value={runningCount} />
          <StatusRow icon={Clock} label="Waiting" value={waitingCount} />
          <StatusRow icon={CircleHelp} label="Questions" value={questionCount} />
          <StatusRow icon={GitMerge} label="Merge Queue" value={mergeQueueCount} />
        </div>

        {/* Navigation */}
        <nav className="flex-1 px-2 py-3 space-y-0.5">
          {navItems.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={to}
              end={to === "/"}
              className={({ isActive }) =>
                cn(
                  "flex items-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors",
                  isActive
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                )
              }
            >
              <Icon className="h-4 w-4" />
              {label}
            </NavLink>
          ))}
        </nav>

        {/* Connection status */}
        <div className="border-t border-border px-4 py-3 text-sm text-muted-foreground">
          <div className="flex items-center gap-2">
            <span
              className={cn(
                "inline-block h-2 w-2 rounded-full",
                connected ? "bg-green-500" : "bg-red-500"
              )}
            />
            {connected ? "Connected" : "Disconnected"}
          </div>
        </div>
      </aside>

      {/* Main content */}
      <main className="flex-1 overflow-auto">
        <Outlet />
      </main>
    </div>
  );
}

function StatusRow({
  icon: Icon,
  label,
  value,
}: {
  icon: typeof MonitorDot;
  label: string;
  value: string | number;
}) {
  return (
    <div className="flex items-center justify-between text-muted-foreground">
      <span className="flex items-center gap-2">
        <Icon className="h-3.5 w-3.5" />
        {label}
      </span>
      <span className="font-mono text-foreground">{value}</span>
    </div>
  );
}
