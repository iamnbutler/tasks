import { useState, useEffect } from "react";
import { Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { createAutomation, type CreateAutomationRequest } from "@/lib/api";
import type { Automation, Project } from "@/lib/types";

interface DuplicateAutomationDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  automation: Automation;
  projects: Project[];
  onSuccess: () => void;
}

export function DuplicateAutomationDialog({
  open,
  onOpenChange,
  automation,
  projects,
  onSuccess,
}: DuplicateAutomationDialogProps) {
  const [targetProjectId, setTargetProjectId] = useState<string>("");
  const [name, setName] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Reset form when dialog opens
  useEffect(() => {
    if (open) {
      // Default to a different project if available, otherwise same project
      const otherProjects = projects.filter((p) => p.id !== automation.project_id);
      setTargetProjectId(otherProjects[0]?.id ?? automation.project_id);
      setName(`${automation.name} (copy)`);
      setError(null);
    }
  }, [open, automation, projects]);

  async function handleDuplicate() {
    if (!targetProjectId) {
      setError("Please select a target project");
      return;
    }
    if (!name.trim()) {
      setError("Name is required");
      return;
    }

    setSaving(true);
    setError(null);

    try {
      const req: CreateAutomationRequest = {
        project_id: targetProjectId,
        name: name.trim(),
        prompt: automation.prompt,
        trigger: automation.trigger,
        state: "active", // Start duplicated automation as active
      };
      await createAutomation(req);
      onOpenChange(false);
      onSuccess();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to duplicate automation");
    } finally {
      setSaving(false);
    }
  }

  // Get project name by ID
  const getProjectName = (projectId: string) => {
    return projects.find((p) => p.id === projectId)?.repo ?? projectId;
  };

  return (
    <Dialog open={open} onOpenChange={(value) => !saving && onOpenChange(value)}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Copy className="h-4 w-4" />
            Duplicate automation
          </DialogTitle>
          <DialogDescription>
            Create a copy of "{automation.name}" in another project.
          </DialogDescription>
        </DialogHeader>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            handleDuplicate();
          }}
        >
          <div className="space-y-4 py-3">
            {/* Source info (read-only) */}
            <div className="rounded-md border border-border bg-muted/30 p-3 space-y-1">
              <p className="text-xs text-muted-foreground">Copying from</p>
              <p className="text-sm font-medium">{getProjectName(automation.project_id)}</p>
            </div>

            {/* Target project selector */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Target Project</label>
              <Select
                value={targetProjectId}
                onValueChange={(v) => {
                  setTargetProjectId(v);
                  setError(null);
                }}
                disabled={saving}
              >
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Select a project" />
                </SelectTrigger>
                <SelectContent>
                  {projects.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.repo}
                      {p.id === automation.project_id && (
                        <span className="ml-2 text-muted-foreground">(same)</span>
                      )}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* Name */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Name</label>
              <Input
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setError(null);
                }}
                disabled={saving}
              />
              <p className="text-xs text-muted-foreground">
                The prompt and trigger settings will be copied.
              </p>
            </div>

            {/* Error message */}
            {error && <p className="text-sm text-red-400">{error}</p>}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={saving}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={saving || !name.trim() || !targetProjectId}>
              {saving ? "Duplicating..." : "Duplicate"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
