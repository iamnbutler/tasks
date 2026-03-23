import { useCallback, useEffect, useRef, useState } from "react";
import { fetchUpdateStatus, subscribeEvents } from "@/lib/api";
import type { UpdateStatus } from "@/lib/types";

const UPDATE_CHECK_INTERVAL_MS = 60_000; // Check every minute

export interface UseUpdateStatusResult {
  status: UpdateStatus | null;
  dismissed: boolean;
  dismiss: () => void;
  refresh: () => Promise<void>;
}

export function useUpdateStatus(): UseUpdateStatusResult {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const lastTargetCommit = useRef<string | null>(null);

  const refresh = useCallback(async () => {
    const newStatus = await fetchUpdateStatus();
    if (newStatus) {
      // If there's a new target commit, reset dismissed state
      if (
        newStatus.target_commit &&
        newStatus.target_commit !== lastTargetCommit.current
      ) {
        setDismissed(false);
        lastTargetCommit.current = newStatus.target_commit;
      }
      setStatus(newStatus);
    }
  }, []);

  const dismiss = useCallback(() => {
    setDismissed(true);
  }, []);

  // Initial fetch and polling
  useEffect(() => {
    refresh();

    const interval = setInterval(() => {
      refresh();
    }, UPDATE_CHECK_INTERVAL_MS);

    return () => clearInterval(interval);
  }, [refresh]);

  // SSE subscription for real-time updates
  useEffect(() => {
    const source = subscribeEvents({ pattern: "system:update*" });

    source.onmessage = (msg) => {
      try {
        const event = JSON.parse(msg.data);

        if (event.type === "system:update_available") {
          // Update available event - refresh status
          const data = event.data as { target_commit?: string; commit_summary?: string; rebuild_scope?: string };
          if (data.target_commit && data.target_commit !== lastTargetCommit.current) {
            setDismissed(false);
            lastTargetCommit.current = data.target_commit;
          }
          refresh();
        } else if (event.type === "system:update_applying") {
          // Update is being applied - could trigger UI refresh
          refresh();
        }
      } catch {
        // Ignore parse errors
      }
    };

    return () => {
      source.close();
    };
  }, [refresh]);

  return {
    status,
    dismissed,
    dismiss,
    refresh,
  };
}
