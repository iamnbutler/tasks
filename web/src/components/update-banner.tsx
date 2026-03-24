import { useState } from "react";
import { Download, RefreshCw, X } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import type { UpdateStatus, RebuildScope } from "@/lib/types";
import { applyUpdate } from "@/lib/api";

const scopeLabels: Record<RebuildScope, string> = {
  frontend: "Frontend",
  server: "Server",
  container: "Container",
};

const scopeDescriptions: Record<RebuildScope, string> = {
  frontend: "Only frontend assets will be rebuilt",
  server: "Server will restart after rebuild",
  container: "Container image will be rebuilt (may take a few minutes)",
};

interface UpdateBannerProps {
  status: UpdateStatus;
  onDismiss?: () => void;
  onUpdateComplete?: () => void;
}

export function UpdateBanner({
  status,
  onDismiss,
  onUpdateComplete,
}: UpdateBannerProps) {
  const [applying, setApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Don't show if no update available and not applying
  if (!status.available && !status.applying && !applying) {
    return null;
  }

  const isApplying = applying || status.applying;

  async function handleApplyUpdate() {
    setApplying(true);
    setError(null);
    try {
      await applyUpdate();
      onUpdateComplete?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to apply update");
      setApplying(false);
    }
  }

  const shortCommit = status.target_commit?.slice(0, 7);

  return (
    <div
      className={cn(
        "flex items-center gap-3 border-b px-4 py-2 text-sm",
        isApplying
          ? "border-yellow-500/30 bg-yellow-500/10"
          : "border-blue-500/30 bg-blue-500/10"
      )}
    >
      {/* Icon */}
      <div className="shrink-0">
        {isApplying ? (
          <RefreshCw className="h-4 w-4 text-yellow-500 animate-spin" />
        ) : (
          <Download className="h-4 w-4 text-blue-500" />
        )}
      </div>

      {/* Content */}
      <div className="flex flex-1 items-center gap-3 min-w-0">
        <span className={cn(isApplying ? "text-yellow-600" : "text-blue-600")}>
          {isApplying ? "Applying update..." : "Update available"}
        </span>

        {shortCommit && (
          <code className="rounded bg-background/50 px-1.5 py-0.5 text-xs text-muted-foreground">
            {shortCommit}
          </code>
        )}

        {status.rebuild_scope && (
          <Badge
            variant="outline"
            className="text-xs"
            title={scopeDescriptions[status.rebuild_scope]}
          >
            {scopeLabels[status.rebuild_scope]}
          </Badge>
        )}

        {status.commit_summary && (
          <span className="truncate text-muted-foreground" title={status.commit_summary}>
            {status.commit_summary}
          </span>
        )}

        {error && <span className="text-xs text-red-400">{error}</span>}
      </div>

      {/* Actions */}
      {!isApplying && (
        <div className="flex items-center gap-2 shrink-0">
          <Button
            size="xs"
            variant="outline"
            onClick={handleApplyUpdate}
            disabled={applying}
            className="border-blue-500/50 text-blue-600 hover:bg-blue-500/20"
          >
            Update Now
          </Button>
          {onDismiss && (
            <Button
              size="icon-xs"
              variant="ghost"
              onClick={onDismiss}
              className="text-muted-foreground hover:text-foreground"
              aria-label="Dismiss"
            >
              <X className="h-3 w-3" />
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
