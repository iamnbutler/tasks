import { useState, useEffect, useRef } from "react";
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

// ---------------------------------------------------------------------------
// DuplicateAutomationDialog
// ---------------------------------------------------------------------------

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
  // Form state
  const [targetProjectId, setTargetProjectId] = useState<string>("");
  const [name, setName] = useState("");

  // UI state
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Reset form when the dialog opens
  const prevOpen = useRef(false);
  useEffect(() => {
    if (open && !prevOpen.current) {
      // Default to a different project if available, otherwise the same project
      const otherProjects = projects.filter((p) => p.id !== automation.project_id);
      setTargetProjectId(otherProjects[0]?.id ?? automation.project_id);
      setName(`${automation.name} (Copy)`);
      setError(null);
    }
    prevOpen.current = open;
  });

  // Validate form
  const validate = (): string | null => {
    if (!targetProjectId) {
      return "Please select a target project";
    }
    if (!name.trim()) {
      return "Name is required";
    }
    return null;
  };

  // Handle form submission
  const handleSubmit = async () => {
    const validationError = validate();
    if (validationError) {
      setError(validationError);
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
        state: automation.state,
      };
      await createAutomation(req);

      onOpenChange(false);
      onSuccess();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to duplicate automation");
    } finally {
      setSaving(false);
    }
  };

  // Find project names for display
  const sourceProject = projects.find((p) => p.id === automation.project_id);
  const targetProject = projects.find((p) => p.id === targetProjectId);

  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!saving) onOpenChange(value);
      }}
    >
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
            handleSubmit();
          }}
        >
          <div className="space-y-4 py-3">
            {/* Source info (read-only) */}
            <div className="rounded-md border border-border bg-muted/30 p-3 space-y-2">
              <div className="text-xs text-muted-foreground">Source</div>
              <div className="text-sm font-medium">{automation.name}</div>
              <div className="text-xs text-muted-foreground">
                {sourceProject?.repo ?? automation.project_id}
              </div>
            </div>

            {/* Target project selector */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Target Project</label>
              <Select
                value={targetProjectId}
                onValueChange={(value) => {
                  setTargetProjectId(value);
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
                        <span className="ml-2 text-muted-foreground">(same project)</span>
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
                placeholder="Automation name"
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setError(null);
                }}
                disabled={saving}
                autoFocus
              />
            </div>

            {/* What will be copied */}
            <div className="text-xs text-muted-foreground">
              The automation's prompt, trigger settings, and state will be copied to the new project.
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
            <Button
              type="submit"
              disabled={saving || !name.trim() || !targetProjectId}
            >
              {saving ? "Duplicating..." : "Duplicate"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
