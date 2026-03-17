import { useState } from "react";
import { Plus } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { createIssue } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export function CreateIssueDialog() {
  const { snapshot, selectedProject } = useAppState();
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [repo, setRepo] = useState<string>("");
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<{ number: number; url: string } | null>(null);

  const projects = snapshot?.projects ?? [];

  // Set default repo when dialog opens
  const handleOpenChange = (newOpen: boolean) => {
    if (newOpen) {
      // If a project is selected, use it as default
      if (selectedProject) {
        const project = projects.find((p) => p.id === selectedProject);
        if (project) {
          setRepo(project.repo);
        }
      } else if (projects.length === 1 && projects[0]) {
        // If only one project, use it
        setRepo(projects[0].repo);
      }
    } else {
      // Reset form when closing
      setTitle("");
      setBody("");
      setRepo("");
      setError(null);
      setSuccess(null);
    }
    setOpen(newOpen);
  };

  const handleCreate = async () => {
    if (!repo) {
      setError("Please select a project");
      return;
    }
    if (!title.trim()) {
      setError("Title is required");
      return;
    }

    setCreating(true);
    setError(null);
    setSuccess(null);

    try {
      const result = await createIssue({
        repo,
        title: title.trim(),
        body: body.trim() || undefined,
      });
      setSuccess(result);
      // Reset form fields but keep success message
      setTitle("");
      setBody("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to create issue");
    } finally {
      setCreating(false);
    }
  };

  const hasProjects = projects.length > 0;

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button size="sm" disabled={!hasProjects}>
          <Plus className="h-4 w-4 mr-1" />
          New Task
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create New Task</DialogTitle>
          <DialogDescription>
            Create a GitHub issue that will be picked up as a task.
          </DialogDescription>
        </DialogHeader>

        {success ? (
          <div className="space-y-4 py-4">
            <div className="rounded-lg border border-green-500/50 bg-green-500/10 p-4">
              <p className="text-sm text-green-400">
                Issue #{success.number} created successfully!
              </p>
              <a
                href={success.url}
                target="_blank"
                rel="noopener noreferrer"
                className="text-sm text-blue-400 hover:underline mt-1 block"
              >
                View on GitHub
              </a>
            </div>
            <p className="text-sm text-muted-foreground">
              The poller will pick up the new issue on its next cycle and create
              a task from it.
            </p>
            <DialogFooter>
              <Button variant="outline" onClick={() => setOpen(false)}>
                Close
              </Button>
              <Button onClick={() => setSuccess(null)}>Create Another</Button>
            </DialogFooter>
          </div>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              handleCreate();
            }}
          >
            <div className="space-y-4 py-4">
              {/* Project selector */}
              <div className="space-y-2">
                <label className="text-sm font-medium">Project</label>
                <Select value={repo} onValueChange={setRepo}>
                  <SelectTrigger className="w-full">
                    <SelectValue placeholder="Select a project" />
                  </SelectTrigger>
                  <SelectContent>
                    {projects.map((project) => (
                      <SelectItem key={project.id} value={project.repo}>
                        {project.repo}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>

              {/* Title input */}
              <div className="space-y-2">
                <label className="text-sm font-medium">Title</label>
                <Input
                  placeholder="Issue title"
                  value={title}
                  onChange={(e) => {
                    setTitle(e.target.value);
                    setError(null);
                  }}
                  disabled={creating}
                  autoFocus
                />
              </div>

              {/* Body textarea */}
              <div className="space-y-2">
                <label className="text-sm font-medium">
                  Description{" "}
                  <span className="text-muted-foreground font-normal">
                    (optional, markdown supported)
                  </span>
                </label>
                <textarea
                  className="flex min-h-[120px] w-full rounded-md border border-input bg-transparent px-3 py-2 text-sm shadow-xs placeholder:text-muted-foreground focus-visible:outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50"
                  placeholder="Describe the task..."
                  value={body}
                  onChange={(e) => setBody(e.target.value)}
                  disabled={creating}
                />
              </div>

              {error && <p className="text-sm text-red-400">{error}</p>}
            </div>

            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => setOpen(false)}
              >
                Cancel
              </Button>
              <Button type="submit" disabled={creating || !title.trim() || !repo}>
                {creating ? "Creating..." : "Create Issue"}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
