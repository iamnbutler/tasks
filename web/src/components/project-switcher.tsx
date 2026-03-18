import { useState } from "react";
import { ChevronsUpDown, Check, Plus, Trash2, FolderGit2 } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { addProject, deleteProject } from "@/lib/api";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";

export function ProjectSwitcher() {
  const { snapshot, refreshSnapshot, selectedProject, setSelectedProject } = useAppState();
  const [open, setOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [newRepo, setNewRepo] = useState("");
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<{ id: string; repo: string } | null>(null);

  const projects = snapshot?.projects ?? [];
  const selectedProjectObj = selectedProject
    ? projects.find((p) => p.id === selectedProject)
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
      // If we deleted the currently selected project, reset to "All Projects"
      if (selectedProject === id) {
        setSelectedProject(null);
      }
      await refreshSnapshot();
      setDeleteConfirm(null);
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : "Failed to delete project");
    }
  };

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="outline"
            role="combobox"
            aria-expanded={open}
            className="w-full justify-between text-sm font-normal"
          >
            <span className="flex items-center gap-2 truncate">
              <FolderGit2 className="h-4 w-4 shrink-0 text-muted-foreground" />
              {projects.length === 0
                ? "No projects"
                : selectedProjectObj
                  ? selectedProjectObj.repo
                  : "All Projects"}
            </span>
            <ChevronsUpDown className="ml-2 h-4 w-4 shrink-0 opacity-50" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[260px] p-0" align="start">
          <Command>
            <CommandInput placeholder="Search projects..." />
            <CommandList>
              <CommandEmpty>No projects found.</CommandEmpty>
              <CommandGroup heading="Projects">
                {/* All Projects option */}
                <CommandItem
                  value="all-projects"
                  className="flex items-center justify-between"
                  onSelect={() => {
                    setSelectedProject(null);
                    setOpen(false);
                  }}
                >
                  <span className="flex items-center gap-2 truncate">
                    <Check
                      className={cn(
                        "h-3.5 w-3.5",
                        !selectedProject ? "opacity-100" : "opacity-0"
                      )}
                    />
                    <span className="truncate">All Projects</span>
                    <span className="text-sm text-muted-foreground">
                      ({projects.length})
                    </span>
                  </span>
                </CommandItem>
                {/* Individual project options */}
                {projects.map((project) => (
                  <CommandItem
                    key={project.id}
                    value={project.repo}
                    className="flex items-center justify-between"
                    onSelect={() => {
                      setSelectedProject(project.id);
                      setOpen(false);
                    }}
                  >
                    <span className="flex items-center gap-2 truncate">
                      <Check
                        className={cn(
                          "h-3.5 w-3.5",
                          selectedProject === project.id ? "opacity-100" : "opacity-0"
                        )}
                      />
                      <span className="truncate">{project.repo}</span>
                    </span>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6 shrink-0 text-muted-foreground hover:text-red-400"
                      onClick={(e) => {
                        e.stopPropagation();
                        setDeleteConfirm({ id: project.id, repo: project.repo });
                      }}
                    >
                      <Trash2 className="h-3 w-3" />
                    </Button>
                  </CommandItem>
                ))}
              </CommandGroup>
              <CommandSeparator />
              <CommandGroup>
                <CommandItem
                  onSelect={() => {
                    setOpen(false);
                    setAddOpen(true);
                  }}
                >
                  <Plus className="mr-2 h-4 w-4" />
                  Add project
                </CommandItem>
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>

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
              {error && (
                <p className="text-sm text-red-400">{error}</p>
              )}
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
              Remove <span className="font-mono font-medium">{deleteConfirm?.repo}</span> from
              tracking? This won't delete the repository.
            </DialogDescription>
          </DialogHeader>
          {deleteError && (
            <p className="text-sm text-red-400">{deleteError}</p>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => { setDeleteConfirm(null); setDeleteError(null); }}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() => deleteConfirm && handleDelete(deleteConfirm.id)}
            >
              Remove
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
