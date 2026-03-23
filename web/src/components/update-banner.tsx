import { useState } from "react";
import { ArrowDownCircle, Loader2, X, CheckCircle2, AlertCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { applyUpdate } from "@/lib/api";
import type { UpdateStatus, UpdateApplyState, RebuildScope } from "@/lib/types";
import { cn } from "@/lib/utils";

interface UpdateBannerProps {
  status: UpdateStatus;
  onDismiss: () => void;
}

function getScopeLabel(scope: RebuildScope): string {
  switch (scope) {
    case "frontend":
      return "Frontend rebuild";
    case "server":
      return "Server restart";
    case "container":
      return "Full rebuild";
    default:
      return "Update";
  }
}

function getScopeDescription(scope: RebuildScope): string {
  switch (scope) {
    case "frontend":
      return "A quick frontend update is available.";
    case "server":
      return "Server restart required.";
    case "container":
      return "Full rebuild including container image required.";
    default:
      return "An update is available.";
  }
}

export function UpdateBanner({ status, onDismiss }: UpdateBannerProps) {
  const [applyState, setApplyState] = useState<UpdateApplyState>("idle");
  const [error, setError] = useState<string | null>(null);

  if (!status.available) {
    return null;
  }

  const handleUpdate = async () => {
    setApplyState("applying");
    setError(null);
    try {
      await applyUpdate();
      setApplyState("success");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Update failed");
      setApplyState("error");
    }
  };

  const isApplying = applyState === "applying";
  const isSuccess = applyState === "success";
  const isError = applyState === "error";

  // Use different colors based on state
  const bannerClasses = cn(
    "flex items-start gap-3 rounded-md border p-3 text-sm",
    isApplying && "border-yellow-500/30 bg-yellow-500/10",
    isSuccess && "border-green-500/30 bg-green-500/10",
    isError && "border-red-500/30 bg-red-500/10",
    !isApplying && !isSuccess && !isError && "border-blue-500/30 bg-blue-500/10"
  );

  const iconClasses = cn(
    "h-5 w-5 shrink-0 mt-0.5",
    isApplying && "text-yellow-400",
    isSuccess && "text-green-400",
    isError && "text-red-400",
    !isApplying && !isSuccess && !isError && "text-blue-400"
  );

  return (
    <div className={bannerClasses}>
      {isApplying ? (
        <Loader2 className={cn(iconClasses, "animate-spin")} />
      ) : isSuccess ? (
        <CheckCircle2 className={iconClasses} />
      ) : isError ? (
        <AlertCircle className={iconClasses} />
      ) : (
        <ArrowDownCircle className={iconClasses} />
      )}

      <div className="flex-1 min-w-0">
        <div className="font-medium">
          {isApplying
            ? "Applying update..."
            : isSuccess
              ? "Update complete"
              : isError
                ? "Update failed"
                : "Update available"}
        </div>

        <div className="text-muted-foreground mt-0.5">
          {isSuccess ? (
            "Reload the page to see changes."
          ) : isError ? (
            error || "An error occurred while applying the update."
          ) : (
            <>
              {status.rebuild_scope && getScopeDescription(status.rebuild_scope)}
              {status.commit_summary && (
                <span className="block mt-1 text-xs font-mono truncate">
                  {status.commit_summary}
                </span>
              )}
            </>
          )}
        </div>

        {status.target_commit && !isApplying && !isSuccess && (
          <div className="mt-1 text-xs text-muted-foreground/70 font-mono">
            {status.current_commit.slice(0, 7)} → {status.target_commit.slice(0, 7)}
            {status.rebuild_scope && (
              <span className="ml-2 px-1.5 py-0.5 rounded bg-accent text-accent-foreground">
                {getScopeLabel(status.rebuild_scope)}
              </span>
            )}
          </div>
        )}
      </div>

      <div className="flex items-center gap-2 shrink-0">
        {!isApplying && !isSuccess && (
          <>
            <Button
              size="sm"
              variant={isError ? "outline" : "default"}
              onClick={handleUpdate}
              className="h-7 text-xs"
            >
              {isError ? "Retry" : "Update Now"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              onClick={onDismiss}
              className="h-7 w-7 p-0"
              aria-label="Dismiss"
            >
              <X className="h-4 w-4" />
            </Button>
          </>
        )}
        {isSuccess && (
          <Button
            size="sm"
            variant="default"
            onClick={() => window.location.reload()}
            className="h-7 text-xs"
          >
            Reload
          </Button>
        )}
      </div>
    </div>
  );
}
