import { useState } from "react";
import { NavLink, Outlet } from "react-router-dom";
import {
  LayoutDashboard,
  ListTodo,
  GitMerge,
  Box,
  Radio,
  Brain,
  ChevronDown,
  FolderGit2,
  Plus,
  Trash2,
  Square,
  Pause,
  Play,
  Rocket,
  Workflow,
  Check,
} from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { addProject, deleteProject, setMode, bootstrapProject } from "@/lib/api";
import { UpdateBanner } from "@/components/update-banner";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
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
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type { Mode } from "@/lib/types";

// ---------------------------------------------------------------------------
// Mode indicator
// ---------------------------------------------------------------------------

const modeConfig: Record<Mode, { label: string; icon: typeof Square }> = {
  stop: { label: "Stopped", icon: Square },
  pause: { label: "Paused", icon: Pause },
  play: { label: "Playing", icon: Play },
};

const modeOrder: Mode[] = ["stop", "pause", "play"];

function ModeSelector() {
  const { snapshot, refreshSnapshot } = useAppState();
  const currentMode = snapshot?.mode ?? "stop";

  async function handleSetMode(mode: Mode) {
    try {
      await setMode(mode);
    } catch {
      // re-sync UI with server state on failure
    }
    refreshSnapshot();
  }

  return (
    <div className="flex items-center gap-1">
      {modeOrder.map((mode) => {
        const config = modeConfig[mode];
        const Icon = config.icon;
        const isActive = currentMode === mode;
        return (
          <Tooltip key={mode}>
            <TooltipTrigger asChild>
              <button
                onClick={() => handleSetMode(mode)}
                className={cn(
                  "flex items-center justify-center h-7 w-7 rounded-md transition-colors",
                  isActive
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                )}
              >
                <Icon className="h-4 w-4" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="top">{config.label}</TooltipContent>
          </Tooltip>
        );
      })}
    </div>
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
// Project selector dropdown
// ---------------------------------------------------------------------------

function ProjectSelector() {
  const { snapshot, refreshSnapshot, selectedProject, setSelectedProject } = useAppState();
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

  // Get display name for the selected project
  const selectedProjectName = selectedProject
    ? projects.find((p) => p.id === selectedProject)?.repo.split("/")[1] ?? "Project"
    : null;

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
      <div className="px-2 py-2 border-b border-border">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button className="flex w-full items-center justify-between gap-2 rounded-md px-2.5 py-2 text-sm bg-accent/50 hover:bg-accent transition-colors">
              <div className="flex items-center gap-2 min-w-0">
                <FolderGit2 className="h-4 w-4 shrink-0" />
                <span className="truncate font-medium">
                  {selectedProjectName ?? "All Projects"}
                </span>
              </div>
              <ChevronDown className="h-4 w-4 shrink-0 text-muted-foreground" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-[calc(var(--radix-dropdown-menu-trigger-width))]">
            {/* All Projects option */}
            <DropdownMenuItem
              onClick={() => setSelectedProject(null)}
              className="flex items-center justify-between"
            >
              <div className="flex items-center gap-2">
                <FolderGit2 className="h-4 w-4" />
                <span>All Projects</span>
              </div>
              <div className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">{projects.length}</span>
                {!selectedProject && <Check className="h-4 w-4" />}
              </div>
            </DropdownMenuItem>

            {projects.length > 0 && <DropdownMenuSeparator />}

            {/* Individual projects */}
            {projects.map((project) => {
              const repoName = project.repo.includes("/")
                ? project.repo.split("/")[1]
                : project.repo;
              const isSelected = selectedProject === project.id;
              return (
                <DropdownMenuItem
                  key={project.id}
                  className="flex items-center justify-between group"
                >
                  <button
                    onClick={() => setSelectedProject(project.id)}
                    className="flex flex-1 items-center gap-2 min-w-0"
                  >
                    <FolderGit2 className="h-4 w-4 shrink-0" />
                    <span className="truncate" title={project.repo}>
                      {repoName}
                    </span>
                  </button>
                  <div className="flex items-center gap-1">
                    {isSelected && <Check className="h-4 w-4" />}
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-5 w-5 shrink-0 opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-red-400 transition-opacity"
                      onClick={(e) => {
                        e.stopPropagation();
                        setDeleteConfirm({ id: project.id, repo: project.repo });
                      }}
                    >
                      <Trash2 className="h-3 w-3" />
                    </Button>
                  </div>
                </DropdownMenuItem>
              );
            })}

            <DropdownMenuSeparator />

            {/* Actions */}
            <DropdownMenuItem onClick={() => setAddOpen(true)}>
              <Plus className="h-4 w-4" />
              <span>Add Project</span>
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => setBootstrapOpen(true)}>
              <Rocket className="h-4 w-4" />
              <span>Bootstrap Project</span>
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>

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
        </div>

        {/* Project selector */}
        <ProjectSelector />

        {/* Navigation */}
        <ScrollArea className="flex-1">
          <div className="flex flex-col gap-4 py-2">
            {/* Main navigation */}
            <div className="space-y-0.5 px-1">
              <SidebarNavItem to="/" icon={LayoutDashboard} label="Dashboard" end />
              <SidebarNavItem to="/tasks" icon={ListTodo} label="Tasks" />
              <SidebarNavItem to="/merge-queue" icon={GitMerge} label="Merge Queue" />
              <SidebarNavItem to="/containers" icon={Box} label="Containers" />
              <SidebarNavItem to="/automations" icon={Workflow} label="Automations" />
            </div>

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

        {/* Footer: mode selector + connection + slots */}
        <div className="border-t border-border px-3 py-2 space-y-2">
          <ModeSelector />
          <div className="flex items-center justify-between text-xs text-muted-foreground">
            <div className="flex items-center gap-1.5">
              <span
                className={cn(
                  "inline-block h-1.5 w-1.5 rounded-full",
                  connected ? "bg-green-500" : "bg-red-500"
                )}
              />
              {connected ? "Connected" : "Disconnected"}
            </div>
            {slotUtil && (
              <span>
                {slotUtil.active}/{slotUtil.max} slots
              </span>
            )}
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
