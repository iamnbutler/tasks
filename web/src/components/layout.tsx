import { useState } from "react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import {
  LayoutDashboard,
  ListTodo,
  GitMerge,
  Box,
  Radio,
  Brain,
  ChevronRight,
  FolderGit2,
  Plus,
  Trash2,
  Square,
  Pause,
  Play,
  Rocket,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { addProject, deleteProject, setMode, bootstrapProject } from "@/lib/api";
import { UpdateBanner } from "@/components/update-banner";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import type { Mode } from "@/lib/types";

// ---------------------------------------------------------------------------
// Mode indicator
// ---------------------------------------------------------------------------

const modeConfig: Record<Mode, { color: string; label: string; icon: typeof Square }> = {
  stop: { color: "bg-red-500", label: "Stopped", icon: Square },
  pause: { color: "bg-yellow-500", label: "Paused", icon: Pause },
  play: { color: "bg-green-500", label: "Playing", icon: Play },
};

const modeOrder: Mode[] = ["stop", "pause", "play"];

function ModeIndicator() {
  const { snapshot, refreshSnapshot } = useAppState();
  const currentMode = snapshot?.mode ?? "stop";
  const config = modeConfig[currentMode];

  async function handleSetMode(mode: Mode) {
    try {
      await setMode(mode);
    } catch {
      // re-sync UI with server state on failure
    }
    refreshSnapshot();
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button className="flex items-center gap-2 rounded-md px-2 py-1 text-sm hover:bg-accent transition-colors">
          <span className={cn("h-2 w-2 rounded-full", config.color)} />
          <span className="text-muted-foreground">{config.label}</span>
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-36">
        {modeOrder.map((mode) => {
          const m = modeConfig[mode];
          const Icon = m.icon;
          return (
            <DropdownMenuItem
              key={mode}
              onClick={() => handleSetMode(mode)}
              className={cn(currentMode === mode && "bg-accent")}
            >
              <Icon className="mr-2 h-3.5 w-3.5" />
              {m.label}
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

// ---------------------------------------------------------------------------
// Sidebar section label
// ---------------------------------------------------------------------------

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <span className="px-3 text-xs font-medium text-muted-foreground/70 uppercase tracking-wider">
      {children}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Sidebar nav item
// ---------------------------------------------------------------------------

function SidebarNavItem({
  to,
  icon: Icon,
  label,
  end,
  badge,
}: {
  to: string;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  end?: boolean;
  badge?: number;
}) {
  return (
    <NavLink
      to={to}
      end={end}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-2 rounded-md px-3 py-1.5 text-sm transition-colors",
          isActive
            ? "bg-accent text-accent-foreground"
            : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
        )
      }
    >
      <Icon className="h-4 w-4 shrink-0" />
      <span className="truncate">{label}</span>
      {badge !== undefined && badge > 0 && (
        <span className="ml-auto text-xs text-muted-foreground">{badge}</span>
      )}
    </NavLink>
  );
}

// ---------------------------------------------------------------------------
// Project list in sidebar
// ---------------------------------------------------------------------------

function ProjectList() {
  const { snapshot, refreshSnapshot, selectedProject, setSelectedProject } = useAppState();
  const navigate = useNavigate();
  const [isOpen, setIsOpen] = useState(true);
  const [addOpen, setAddOpen] = useState(false);
  const [newRepo, setNewRepo] = useState("");
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<{ id: string; repo: string } | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  // Bootstrap project state
  const [bootstrapOpen, setBootstrapOpen] = useState(false);
  const [bootstrapPrompt, setBootstrapPrompt] = useState("");
  const [bootstrapRepoName, setBootstrapRepoName] = useState("");
  const [bootstrapping, setBootstrapping] = useState(false);
  const [bootstrapError, setBootstrapError] = useState<string | null>(null);
  const [bootstrapResult, setBootstrapResult] = useState<{ repoUrl: string; issueUrl: string } | null>(null);

  const projects = snapshot?.projects ?? [];

  const handleAdd = async () => {
    const repo = newRepo.trim();
    if (!repo) return;
    if (!repo.includes("/") || repo.split("/").length !== 2) {
      setError("Format: owner/repo");
      return;
    }
    setAdding(true);
    setError(null);
    try {
      await addProject(repo);
      await refreshSnapshot();
      setNewRepo("");
      setAddOpen(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to add project");
    } finally {
      setAdding(false);
    }
  };

  const handleDelete = async (id: string) => {
    setDeleteError(null);
    try {
      await deleteProject(id);
      if (selectedProject === id) {
        setSelectedProject(null);
      }
      await refreshSnapshot();
      setDeleteConfirm(null);
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : "Failed to delete project");
    }
  };

  const handleProjectClick = (projectId: string) => {
    if (selectedProject === projectId) {
      setSelectedProject(null);
    } else {
      setSelectedProject(projectId);
    }
    navigate("/tasks");
  };

  const handleBootstrap = async () => {
    const prompt = bootstrapPrompt.trim();
    if (!prompt) return;

    setBootstrapping(true);
    setBootstrapError(null);
    setBootstrapResult(null);
    try {
      const result = await bootstrapProject({
        prompt,
        repo_name: bootstrapRepoName.trim() || undefined,
      });
      await refreshSnapshot();
      setSelectedProject(result.project.id);
      setBootstrapResult({
        repoUrl: result.repo_url,
        issueUrl: result.issue.url,
      });
      // Clear the form but keep the dialog open to show the result
      setBootstrapPrompt("");
      setBootstrapRepoName("");
    } catch (e) {
      setBootstrapError(e instanceof Error ? e.message : "Failed to bootstrap project");
    } finally {
      setBootstrapping(false);
    }
  };

  const resetBootstrapDialog = () => {
    setBootstrapOpen(false);
    setBootstrapPrompt("");
    setBootstrapRepoName("");
    setBootstrapError(null);
    setBootstrapResult(null);
  };

  return (
    <>
      <Collapsible open={isOpen} onOpenChange={setIsOpen}>
        <div className="flex items-center justify-between px-3 py-1">
          <CollapsibleTrigger className="flex items-center gap-1 text-xs font-medium text-muted-foreground/70 uppercase tracking-wider hover:text-muted-foreground transition-colors">
            <ChevronRight
              className={cn(
                "h-3 w-3 transition-transform",
                isOpen && "rotate-90"
              )}
            />
            Projects
          </CollapsibleTrigger>
          <div className="flex items-center gap-0.5">
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-5 w-5 text-muted-foreground/70 hover:text-foreground"
                  onClick={() => setBootstrapOpen(true)}
                >
                  <Rocket className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="right">Bootstrap new project</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-5 w-5 text-muted-foreground/70 hover:text-foreground"
                  onClick={() => setAddOpen(true)}
                >
                  <Plus className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="right">Add existing project</TooltipContent>
            </Tooltip>
          </div>
        </div>

        <CollapsibleContent className="space-y-0.5 px-1">
          {/* All Projects option */}
          <button
            onClick={() => {
              setSelectedProject(null);
              navigate("/tasks");
            }}
            className={cn(
              "flex w-full items-center gap-2 rounded-md px-3 py-1.5 text-sm transition-colors",
              !selectedProject
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
            )}
          >
            <FolderGit2 className="h-4 w-4 shrink-0" />
            <span className="truncate">All Projects</span>
            <span className="ml-auto text-xs text-muted-foreground">
              {projects.length}
            </span>
          </button>

          {projects.map((project) => {
            const repoName = project.repo.includes("/")
              ? project.repo.split("/")[1]
              : project.repo;
            return (
              <div
                key={project.id}
                className={cn(
                  "group flex items-center rounded-md transition-colors",
                  selectedProject === project.id
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                )}
              >
                <button
                  onClick={() => handleProjectClick(project.id)}
                  className="flex flex-1 items-center gap-2 px-3 py-1.5 text-sm min-w-0"
                >
                  <FolderGit2 className="h-4 w-4 shrink-0" />
                  <span className="truncate" title={project.repo}>
                    {repoName}
                  </span>
                </button>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6 shrink-0 mr-1 opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-red-400 transition-opacity"
                  onClick={(e) => {
                    e.stopPropagation();
                    setDeleteConfirm({ id: project.id, repo: project.repo });
                  }}
                >
                  <Trash2 className="h-3 w-3" />
                </Button>
              </div>
            );
          })}
        </CollapsibleContent>
      </Collapsible>

      {/* Add project dialog */}
      <Dialog open={addOpen} onOpenChange={setAddOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Add project</DialogTitle>
            <DialogDescription>
              Enter a GitHub repository in owner/repo format.
            </DialogDescription>
          </DialogHeader>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              handleAdd();
            }}
          >
            <div className="space-y-2 py-3">
              <Input
                placeholder="owner/repo"
                value={newRepo}
                onChange={(e) => {
                  setNewRepo(e.target.value);
                  setError(null);
                }}
                disabled={adding}
                autoFocus
              />
              {error && <p className="text-sm text-red-400">{error}</p>}
            </div>
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => setAddOpen(false)}
              >
                Cancel
              </Button>
              <Button type="submit" disabled={adding || !newRepo.trim()}>
                {adding ? "Adding..." : "Add"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* Delete confirmation dialog */}
      <Dialog
        open={deleteConfirm !== null}
        onOpenChange={(open) => !open && setDeleteConfirm(null)}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>Remove project</DialogTitle>
            <DialogDescription>
              Remove{" "}
              <span className="font-mono font-medium">
                {deleteConfirm?.repo}
              </span>{" "}
              from tracking? This won't delete the repository.
            </DialogDescription>
          </DialogHeader>
          {deleteError && (
            <p className="text-sm text-red-400">{deleteError}</p>
          )}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => {
                setDeleteConfirm(null);
                setDeleteError(null);
              }}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() =>
                deleteConfirm && handleDelete(deleteConfirm.id)
              }
            >
              Remove
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bootstrap project dialog */}
      <Dialog open={bootstrapOpen} onOpenChange={(open) => !open && resetBootstrapDialog()}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Bootstrap new project</DialogTitle>
            <DialogDescription>
              Describe what you want to build. A new private repository will be created
              and an agent will start working on your idea.
            </DialogDescription>
          </DialogHeader>
          {bootstrapResult ? (
            <div className="space-y-4 py-3">
              <div className="rounded-md bg-green-500/10 border border-green-500/20 p-4 space-y-2">
                <p className="text-sm font-medium text-green-400">Project created!</p>
                <div className="space-y-1 text-sm text-muted-foreground">
                  <p>
                    <span className="text-foreground">Repository: </span>
                    <a
                      href={bootstrapResult.repoUrl}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-blue-400 hover:underline"
                    >
                      {bootstrapResult.repoUrl}
                    </a>
                  </p>
                  <p>
                    <span className="text-foreground">Initial issue: </span>
                    <a
                      href={bootstrapResult.issueUrl}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-blue-400 hover:underline"
                    >
                      {bootstrapResult.issueUrl}
                    </a>
                  </p>
                </div>
                <p className="text-xs text-muted-foreground pt-2">
                  The agent will pick up the task shortly. Create additional issues
                  in the repository for questions or clarifications.
                </p>
              </div>
              <DialogFooter>
                <Button onClick={resetBootstrapDialog}>Done</Button>
              </DialogFooter>
            </div>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                handleBootstrap();
              }}
            >
              <div className="space-y-4 py-3">
                <div className="space-y-2">
                  <label htmlFor="prompt" className="text-sm font-medium">
                    What do you want to build?
                  </label>
                  <Textarea
                    id="prompt"
                    placeholder="Describe your project idea in detail. What should it do? What technologies should it use?"
                    value={bootstrapPrompt}
                    onChange={(e) => {
                      setBootstrapPrompt(e.target.value);
                      setBootstrapError(null);
                    }}
                    disabled={bootstrapping}
                    className="min-h-32"
                    autoFocus
                  />
                </div>
                <div className="space-y-2">
                  <label htmlFor="repoName" className="text-sm font-medium">
                    Repository name{" "}
                    <span className="text-muted-foreground font-normal">(optional)</span>
                  </label>
                  <Input
                    id="repoName"
                    placeholder="my-awesome-project"
                    value={bootstrapRepoName}
                    onChange={(e) => {
                      setBootstrapRepoName(e.target.value);
                      setBootstrapError(null);
                    }}
                    disabled={bootstrapping}
                  />
                  <p className="text-xs text-muted-foreground">
                    Leave empty to derive from your description.
                  </p>
                </div>
                {bootstrapError && (
                  <p className="text-sm text-red-400">{bootstrapError}</p>
                )}
              </div>
              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  onClick={resetBootstrapDialog}
                  disabled={bootstrapping}
                >
                  Cancel
                </Button>
                <Button
                  type="submit"
                  disabled={bootstrapping || !bootstrapPrompt.trim()}
                >
                  {bootstrapping ? "Creating..." : "Create project"}
                </Button>
              </DialogFooter>
            </form>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

export function Layout() {
  const { connected, snapshot, updateStatus, updateDismissed, dismissUpdate, refreshUpdateStatus } = useAppState();
  const slotUtil = snapshot?.slot_utilization;

  // Show update banner if update is available (or applying) and not dismissed
  const showUpdateBanner = updateStatus && (updateStatus.available || updateStatus.applying) && !updateDismissed;

  return (
    <div className="flex h-screen bg-background text-foreground">
      {/* Sidebar */}
      <aside className="flex w-56 flex-col border-r border-border bg-background">
        {/* Header */}
        <div className="flex items-center justify-between px-3 py-2.5 border-b border-border">
          <div className="flex items-center gap-2">
            <div className="flex h-6 w-6 items-center justify-center rounded-md bg-accent">
              <ListTodo className="h-3.5 w-3.5" />
            </div>
            <span className="text-sm font-semibold">Tasks</span>
          </div>
          <ModeIndicator />
        </div>

        {/* Navigation */}
        <ScrollArea className="flex-1">
          <div className="flex flex-col gap-4 py-2">
            {/* Main navigation */}
            <div className="space-y-0.5 px-1">
              <SidebarNavItem to="/" icon={LayoutDashboard} label="Dashboard" end />
              <SidebarNavItem to="/tasks" icon={ListTodo} label="Tasks" />
              <SidebarNavItem to="/merge-queue" icon={GitMerge} label="Merge Queue" />
              <SidebarNavItem to="/containers" icon={Box} label="Containers" />
            </div>

            {/* Projects */}
            <ProjectList />

            {/* System */}
            <div className="space-y-1">
              <SectionLabel>System</SectionLabel>
              <div className="space-y-0.5 px-1">
                <SidebarNavItem to="/orchestrator" icon={Brain} label="Orchestrator" />
                <SidebarNavItem to="/events" icon={Radio} label="Events" />
              </div>
            </div>
          </div>
        </ScrollArea>

        {/* Footer: connection + slots */}
        <div className="border-t border-border px-3 py-2 space-y-1">
          {slotUtil && (
            <div className="flex items-center justify-between text-xs text-muted-foreground">
              <span>Slots</span>
              <span>
                {slotUtil.active}/{slotUtil.max}
              </span>
            </div>
          )}
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
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
      <main className="flex flex-1 flex-col overflow-hidden">
        {showUpdateBanner && (
          <UpdateBanner
            status={updateStatus}
            onDismiss={dismissUpdate}
            onUpdateComplete={refreshUpdateStatus}
          />
        )}
        <div className="flex-1 overflow-auto">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
