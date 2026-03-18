import { NavLink, Outlet } from "react-router-dom";
import { ListTodo, GitMerge, Radio, Brain } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { ModeControl } from "@/components/mode-control";
import { ProjectSwitcher } from "@/components/project-switcher";
import { cn } from "@/lib/utils";

const navItems = [
  { to: "/", label: "Tasks", icon: ListTodo },
  { to: "/merge-queue", label: "Merge Queue", icon: GitMerge },
  { to: "/orchestrator", label: "Orchestrator", icon: Brain },
  { to: "/events", label: "Events", icon: Radio },
];

export function Layout() {
  const { connected } = useAppState();

  return (
    <div className="flex h-screen bg-background text-foreground">
      {/* Sidebar */}
      <aside className="flex w-48 flex-col border-r border-border bg-background">
        {/* Header: title + mode control */}
        <div className="flex items-center justify-between border-b border-border px-3 py-2">
          <h1 className="text-base font-bold tracking-tight">Tasks</h1>
          <ModeControl />
        </div>

        {/* Project switcher */}
        <div className="border-b border-border px-3 py-2">
          <ProjectSwitcher />
        </div>

        {/* Navigation */}
        <nav className="flex-1 px-2 py-2 space-y-0.5">
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
        <div className="border-t border-border px-3 py-1.5 text-xs text-muted-foreground">
          <div className="flex items-center gap-1.5">
            <span
              className={cn(
                "inline-block h-1.5 w-1.5 rounded-full",
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

