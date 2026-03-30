import { useState, useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
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
import {
  createAutomation,
  updateAutomation,
  type CreateAutomationRequest,
  type UpdateAutomationRequest,
} from "@/lib/api";
import type { Automation, TriggerType, Project } from "@/lib/types";

// ---------------------------------------------------------------------------
// Cron presets
// ---------------------------------------------------------------------------

interface CronPreset {
  label: string;
  value: string;
}

const CRON_PRESETS: CronPreset[] = [
  { label: "Hourly", value: "0 * * * *" },
  { label: "Daily at 9am", value: "0 9 * * *" },
  { label: "Weekly (Monday 9am)", value: "0 9 * * 1" },
  { label: "Every 6 hours", value: "0 */6 * * *" },
  { label: "Custom", value: "custom" },
];

// ---------------------------------------------------------------------------
// Event type options
// ---------------------------------------------------------------------------

interface EventTypeOption {
  label: string;
  value: string;
}

const EVENT_TYPES: EventTypeOption[] = [
  { label: "PR Opened", value: "pr_opened" },
  { label: "PR Updated", value: "pr_updated" },
  { label: "PR Merged", value: "pr_merged" },
  { label: "Issue Created", value: "issue_created" },
  { label: "Issue Updated", value: "issue_updated" },
  { label: "Push to Branch", value: "push" },
];

// ---------------------------------------------------------------------------
// Example prompts
// ---------------------------------------------------------------------------

const EXAMPLE_PROMPTS = [
  "Review every PR for committed .env files or secrets",
  "Check if documentation is up to date with code changes",
  "Analyze performance and suggest optimizations",
];

// ---------------------------------------------------------------------------
// Cron validation helper
// ---------------------------------------------------------------------------

function isValidCron(cron: string): boolean {
  // Basic cron validation: 5 space-separated fields
  const parts = cron.trim().split(/\s+/);
  if (parts.length !== 5) return false;

  // Basic pattern check for each field (allows numbers, *, /, -, ,)
  const fieldPattern = /^(\*|[0-9]+(-[0-9]+)?)(\/[0-9]+)?(,(\*|[0-9]+(-[0-9]+)?)(\/[0-9]+)?)*$/;
  return parts.every((part) => fieldPattern.test(part));
}

// ---------------------------------------------------------------------------
// AutomationFormDialog
// ---------------------------------------------------------------------------

interface AutomationFormDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  projects: Project[];
  selectedProjectId?: string;
  automation?: Automation; // If provided, edit mode
  onSuccess: () => void;
}

export function AutomationFormDialog({
  open,
  onOpenChange,
  projects,
  selectedProjectId,
  automation,
  onSuccess,
}: AutomationFormDialogProps) {
  const isEditMode = !!automation;

  // Form state
  const [projectId, setProjectId] = useState<string>("");
  const [name, setName] = useState("");
  const [prompt, setPrompt] = useState("");
  const [triggerType, setTriggerType] = useState<TriggerType>("manual");
  const [cronPreset, setCronPreset] = useState<string>("0 9 * * *");
  const [customCron, setCustomCron] = useState("");
  const [eventType, setEventType] = useState<string>("pr_opened");
  const [isActive, setIsActive] = useState(true);

  // UI state
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Reset form only when the dialog transitions from closed → open
  const prevOpen = useRef(false);
  useEffect(() => {
    if (open && !prevOpen.current) {
      if (automation) {
        // Edit mode: populate form with existing data
        setProjectId(automation.project_id);
        setName(automation.name);
        setPrompt(automation.prompt);
        setTriggerType(automation.trigger.type);
        setIsActive(automation.state === "active");

        if (automation.trigger.type === "schedule" && automation.trigger.cron) {
          const preset = CRON_PRESETS.find(
            (p) => p.value === automation.trigger.cron
          );
          if (preset && preset.value !== "custom") {
            setCronPreset(automation.trigger.cron);
            setCustomCron("");
          } else {
            setCronPreset("custom");
            setCustomCron(automation.trigger.cron);
          }
        }

        if (automation.trigger.type === "event" && automation.trigger.event_type) {
          setEventType(automation.trigger.event_type);
        }
      } else {
        // Create mode: reset to defaults
        setProjectId(selectedProjectId ?? projects[0]?.id ?? "");
        setName("");
        setPrompt("");
        setTriggerType("manual");
        setCronPreset("0 9 * * *");
        setCustomCron("");
        setEventType("pr_opened");
        setIsActive(true);
      }
      setError(null);
    }
    prevOpen.current = open;
  });

  // Get the actual cron value (handles custom preset)
  const getCronValue = (): string => {
    return cronPreset === "custom" ? customCron : cronPreset;
  };

  // Validate form
  const validate = (): string | null => {
    if (!projectId) {
      return "Please select a project";
    }
    if (!name.trim()) {
      return "Name is required";
    }
    if (!prompt.trim()) {
      return "Prompt is required";
    }
    if (prompt.trim().length < 10) {
      return "Prompt must be at least 10 characters";
    }
    if (triggerType === "schedule") {
      const cronValue = getCronValue();
      if (!cronValue.trim()) {
        return "Cron expression is required for schedule triggers";
      }
      if (!isValidCron(cronValue)) {
        return "Invalid cron expression. Expected 5 space-separated fields (minute hour day month weekday)";
      }
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
      const trigger = {
        type: triggerType,
        ...(triggerType === "schedule" && { cron: getCronValue() }),
        ...(triggerType === "event" && { event_type: eventType }),
      };

      if (isEditMode && automation) {
        const updates: UpdateAutomationRequest = {
          name: name.trim(),
          prompt: prompt.trim(),
          trigger,
          state: isActive ? "active" : "paused",
        };
        await updateAutomation(automation.id, updates);
      } else {
        const req: CreateAutomationRequest = {
          project_id: projectId,
          name: name.trim(),
          prompt: prompt.trim(),
          trigger,
          state: isActive ? "active" : "paused",
        };
        await createAutomation(req);
      }

      onOpenChange(false);
      onSuccess();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : `Failed to ${isEditMode ? "update" : "create"} automation`
      );
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!saving) onOpenChange(value);
      }}
    >
      <DialogContent className="sm:max-w-lg max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>
            {isEditMode ? "Edit automation" : "Create automation"}
          </DialogTitle>
          <DialogDescription>
            {isEditMode
              ? "Update the automation configuration."
              : "Define an automation with a natural language prompt."}
          </DialogDescription>
        </DialogHeader>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            handleSubmit();
          }}
        >
          <div className="space-y-4 py-3">
            {/* Project selector (disabled in edit mode) */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Project</label>
              <Select
                value={projectId}
                onValueChange={setProjectId}
                disabled={isEditMode || saving}
              >
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Select a project" />
                </SelectTrigger>
                <SelectContent>
                  {projects.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.repo}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* Name */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Name</label>
              <Input
                placeholder="e.g., PR Secret Scanner"
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setError(null);
                }}
                disabled={saving}
                autoFocus
              />
            </div>

            {/* Prompt */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Prompt</label>
              <Textarea
                placeholder="Describe what this automation should do..."
                value={prompt}
                onChange={(e) => {
                  setPrompt(e.target.value);
                  setError(null);
                }}
                disabled={saving}
                className="min-h-28"
              />
              <div className="text-xs text-muted-foreground">
                <p className="mb-1">Example prompts:</p>
                <ul className="list-disc list-inside space-y-0.5">
                  {EXAMPLE_PROMPTS.map((example, i) => (
                    <li
                      key={i}
                      className="cursor-pointer hover:text-foreground transition-colors"
                      onClick={() => {
                        if (!saving) {
                          setPrompt(example);
                          setError(null);
                        }
                      }}
                    >
                      {example}
                    </li>
                  ))}
                </ul>
              </div>
            </div>

            {/* Trigger Type */}
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Trigger Type</label>
              <Select
                value={triggerType}
                onValueChange={(v) => setTriggerType(v as TriggerType)}
                disabled={saving}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="manual">Manual</SelectItem>
                  <SelectItem value="schedule">Schedule</SelectItem>
                  <SelectItem value="event">Event</SelectItem>
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">
                {triggerType === "manual" &&
                  "Run this automation manually when needed."}
                {triggerType === "schedule" &&
                  "Run this automation on a recurring schedule."}
                {triggerType === "event" &&
                  "Run this automation when specific events occur."}
              </p>
            </div>

            {/* Schedule Configuration */}
            {triggerType === "schedule" && (
              <div className="space-y-3 rounded-md border border-border p-3">
                <div className="space-y-1.5">
                  <label className="text-xs font-medium">Schedule</label>
                  <Select
                    value={cronPreset}
                    onValueChange={(v) => {
                      setCronPreset(v);
                      if (v !== "custom") {
                        setCustomCron("");
                      }
                      setError(null);
                    }}
                    disabled={saving}
                  >
                    <SelectTrigger className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {CRON_PRESETS.map((preset) => (
                        <SelectItem key={preset.value} value={preset.value}>
                          {preset.label}
                          {preset.value !== "custom" && (
                            <span className="ml-2 font-mono text-muted-foreground">
                              {preset.value}
                            </span>
                          )}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                {cronPreset === "custom" && (
                  <div className="space-y-1.5">
                    <label className="text-xs font-medium">
                      Cron Expression
                    </label>
                    <Input
                      placeholder="* * * * *"
                      value={customCron}
                      onChange={(e) => {
                        setCustomCron(e.target.value);
                        setError(null);
                      }}
                      disabled={saving}
                      className="font-mono"
                    />
                    <p className="text-xs text-muted-foreground">
                      Format: minute hour day month weekday
                    </p>
                  </div>
                )}
              </div>
            )}

            {/* Event Configuration */}
            {triggerType === "event" && (
              <div className="space-y-1.5 rounded-md border border-border p-3">
                <label className="text-xs font-medium">Event Type</label>
                <Select
                  value={eventType}
                  onValueChange={setEventType}
                  disabled={saving}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {EVENT_TYPES.map((event) => (
                      <SelectItem key={event.value} value={event.value}>
                        {event.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            )}

            {/* State Toggle */}
            <div className="flex items-center justify-between rounded-md border border-border p-3">
              <div className="space-y-0.5">
                <label className="text-xs font-medium">Active</label>
                <p className="text-xs text-muted-foreground">
                  {isActive
                    ? "Automation will run when triggered"
                    : "Automation is paused and won't run"}
                </p>
              </div>
              <Switch
                checked={isActive}
                onCheckedChange={setIsActive}
                disabled={saving}
              />
            </div>

            {/* Compiled workflow (read-only, edit mode only) */}
            {isEditMode && automation?.compiled_workflow && (
              <div className="space-y-1.5">
                <label className="text-xs font-medium">Compiled Workflow</label>
                <div className="rounded-md border border-border bg-muted/50 p-3">
                  <pre className="text-xs whitespace-pre-wrap text-muted-foreground">
                    {automation.compiled_workflow}
                  </pre>
                </div>
                <p className="text-xs text-muted-foreground">
                  This is the compiled version of your prompt (read-only).
                  {automation.compiled_at && ` Compiled: ${new Date(automation.compiled_at).toLocaleString()}.`}
                  {automation.compiled_at && new Date(automation.updated_at) > new Date(automation.compiled_at) && (
                    <span className="text-warning"> Stale — automation was updated after last compilation.</span>
                  )}
                </p>
              </div>
            )}

            {/* Updated timestamp (edit mode only) */}
            {isEditMode && automation && (
              <p className="text-xs text-muted-foreground">
                Last updated:{" "}
                {new Date(automation.updated_at).toLocaleString()}
              </p>
            )}

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
              disabled={saving || !name.trim() || !prompt.trim() || !projectId}
            >
              {saving
                ? isEditMode
                  ? "Saving..."
                  : "Creating..."
                : isEditMode
                  ? "Save Changes"
                  : "Create Automation"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
