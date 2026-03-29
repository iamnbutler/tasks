import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";
import { createElement } from "react";
import type { Automation, Event, Snapshot, Task, MergeQueueEntry, UpdateStatus } from "@/lib/types";
import { fetchAutomations, fetchSnapshot, fetchUpdateStatus, subscribeEvents } from "@/lib/api";

const MAX_EVENTS = 200;
const POLL_INTERVAL_MS = 5_000;

/** Regex matching event types that should trigger a snapshot refresh. */
const STATE_CHANGING_EVENT = /^(task:|merge:|reflection:|system:mode)/;

/** Regex matching event types that should trigger an automations refresh. */
const AUTOMATION_EVENT = /^automation:/;

/** Regex matching event types that should trigger an update status refresh. */
const UPDATE_EVENT = /^system:update:/;

export interface AppState {
  snapshot: Snapshot | null;
  events: Event[];
  /** Orchestrator events (preserved indefinitely, not subject to MAX_EVENTS cap) */
  orchestratorEvents: Event[];
  connected: boolean;
  error: Error | null;
  refreshSnapshot: () => Promise<void>;
  /** Currently selected project ID (null = all projects) */
  selectedProject: string | null;
  setSelectedProject: (id: string | null) => void;
  /** Tasks filtered by selected project */
  filteredTasks: Task[];
  /** Merge queue entries filtered by selected project */
  filteredMergeQueue: MergeQueueEntry[];
  /** All automations */
  automations: Automation[];
  /** Automations filtered by selected project */
  filteredAutomations: Automation[];
  /** Refresh automations list */
  refreshAutomations: () => Promise<void>;
  /** Update status for self-update mechanism */
  updateStatus: UpdateStatus | null;
  /** Whether the update banner has been dismissed */
  updateDismissed: boolean;
  /** Dismiss the update banner (reappears on next check) */
  dismissUpdate: () => void;
  /** Refresh update status */
  refreshUpdateStatus: () => Promise<void>;
}

const defaultState: AppState = {
  snapshot: null,
  events: [],
  orchestratorEvents: [],
  connected: false,
  error: null,
  refreshSnapshot: async () => {},
  selectedProject: null,
  setSelectedProject: () => {},
  filteredTasks: [],
  filteredMergeQueue: [],
  automations: [],
  filteredAutomations: [],
  refreshAutomations: async () => {},
  updateStatus: null,
  updateDismissed: false,
  dismissUpdate: () => {},
  refreshUpdateStatus: async () => {},
};

export const AppStateContext = createContext<AppState | null>(null);

// ---------------------------------------------------------------------------
// Core hook – manages polling, SSE, events list
// ---------------------------------------------------------------------------

function useAppStateCore(): AppState {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [orchestratorEvents, setOrchestratorEvents] = useState<Event[]>([]);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [selectedProject, setSelectedProject] = useState<string | null>(null);
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus | null>(null);
  const [updateDismissed, setUpdateDismissed] = useState(false);
  const [automations, setAutomations] = useState<Automation[]>([]);

  // Track seen orchestrator event IDs for deduplication
  const seenOrchestratorIds = useRef(new Set<string>());

  // Keep a ref to the latest snapshot-fetch promise so we can avoid races.
  const fetchInFlight = useRef(false);
  const updateFetchInFlight = useRef(false);
  const automationsFetchInFlight = useRef(false);
  const prevTargetCommit = useRef<string | undefined>(undefined);

  const refreshSnapshot = useCallback(async () => {
    if (fetchInFlight.current) return;
    fetchInFlight.current = true;
    try {
      const snap = await fetchSnapshot();
      setSnapshot(snap);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      fetchInFlight.current = false;
    }
  }, []);

  const refreshUpdateStatus = useCallback(async () => {
    if (updateFetchInFlight.current) return;
    updateFetchInFlight.current = true;
    try {
      const status = await fetchUpdateStatus();
      setUpdateStatus(status);
      // Reset dismissed state when a new update becomes available
      if (status.available && status.target_commit !== prevTargetCommit.current) {
        setUpdateDismissed(false);
      }
      prevTargetCommit.current = status.target_commit;
    } catch {
      // Update status endpoint may not exist yet; ignore errors silently
    } finally {
      updateFetchInFlight.current = false;
    }
  }, []);

  const dismissUpdate = useCallback(() => {
    setUpdateDismissed(true);
  }, []);

  const refreshAutomations = useCallback(async () => {
    if (automationsFetchInFlight.current) return;
    automationsFetchInFlight.current = true;
    try {
      const data = await fetchAutomations();
      setAutomations(data);
    } catch {
      // Automations endpoint may not exist yet; ignore errors silently
    } finally {
      automationsFetchInFlight.current = false;
    }
  }, []);

  // Compute filtered tasks based on selected project
  const filteredTasks = useMemo(() => {
    const tasks = snapshot?.tasks ?? [];
    if (!selectedProject) return tasks;
    return tasks.filter((t) => t.project === selectedProject);
  }, [snapshot, selectedProject]);

  // Compute filtered merge queue entries based on selected project
  const filteredMergeQueue = useMemo(() => {
    const entries = snapshot?.merge_queue ?? [];
    if (!selectedProject) return entries;
    const tasks = snapshot?.tasks ?? [];
    // Create a set of task IDs that belong to the selected project
    const projectTaskIds = new Set(
      tasks.filter((t) => t.project === selectedProject).map((t) => t.id)
    );
    return entries.filter((e) => projectTaskIds.has(e.task_id));
  }, [snapshot, selectedProject]);

  // Compute filtered automations based on selected project
  const filteredAutomations = useMemo(() => {
    if (!selectedProject) return automations;
    return automations.filter((a) => a.project_id === selectedProject);
  }, [automations, selectedProject]);

  // --- Polling -----------------------------------------------------------
  useEffect(() => {
    // Fetch immediately on mount.
    refreshSnapshot();
    refreshUpdateStatus();
    refreshAutomations();

    const interval = setInterval(() => {
      refreshSnapshot();
    }, POLL_INTERVAL_MS);

    // Automations are polled at the same interval as snapshot
    const automationsInterval = setInterval(() => {
      refreshAutomations();
    }, POLL_INTERVAL_MS);

    // Update status is checked less frequently (every 60 seconds)
    const updateInterval = setInterval(() => {
      refreshUpdateStatus();
    }, 60_000);

    return () => {
      clearInterval(interval);
      clearInterval(automationsInterval);
      clearInterval(updateInterval);
    };
  }, [refreshSnapshot, refreshUpdateStatus, refreshAutomations]);

  // --- SSE subscription with reconnection --------------------------------
  useEffect(() => {
    let source: EventSource | null = null;
    let disconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let closed = false;

    /** Grace period before showing "Disconnected" — EventSource auto-reconnects,
     *  so brief hiccups shouldn't flash the indicator. */
    const DISCONNECT_GRACE_MS = 3_000;

    function connect() {
      if (closed) return;
      source = subscribeEvents();

      source.onopen = () => {
        if (disconnectTimer) {
          clearTimeout(disconnectTimer);
          disconnectTimer = null;
        }
        setConnected(true);
        setError(null);
      };

      source.onmessage = (msg) => {
        try {
          const event: Event = JSON.parse(msg.data);

          setEvents((prev) => {
            const next = [event, ...prev];
            return next.length > MAX_EVENTS ? next.slice(0, MAX_EVENTS) : next;
          });

          // Accumulate orchestrator events separately (no cap, persists across navigation)
          if (event.type.startsWith("orchestrator:")) {
            if (!seenOrchestratorIds.current.has(event.id)) {
              seenOrchestratorIds.current.add(event.id);
              setOrchestratorEvents((prev) => [...prev, event]);
            }
          }

          if (STATE_CHANGING_EVENT.test(event.type)) {
            refreshSnapshot();
          }

          // Handle automation events
          if (AUTOMATION_EVENT.test(event.type)) {
            refreshAutomations();
          }

          // Handle update events
          if (UPDATE_EVENT.test(event.type)) {
            refreshUpdateStatus();
          }
        } catch {
          // Ignore unparseable messages.
        }
      };

      source.onerror = () => {
        // Don't immediately mark disconnected — EventSource will auto-retry.
        // Only show disconnected if it stays down past the grace period.
        if (!disconnectTimer) {
          disconnectTimer = setTimeout(() => {
            disconnectTimer = null;
            setConnected(false);
            setError(new Error("SSE connection lost"));
          }, DISCONNECT_GRACE_MS);
        }
      };
    }

    connect();

    return () => {
      closed = true;
      if (disconnectTimer) clearTimeout(disconnectTimer);
      source?.close();
      setConnected(false);
    };
  }, [refreshSnapshot, refreshAutomations, refreshUpdateStatus]);

  return {
    snapshot,
    events,
    orchestratorEvents,
    connected,
    error,
    refreshSnapshot,
    selectedProject,
    setSelectedProject,
    filteredTasks,
    filteredMergeQueue,
    automations,
    filteredAutomations,
    refreshAutomations,
    updateStatus,
    updateDismissed,
    dismissUpdate,
    refreshUpdateStatus,
  };
}

// ---------------------------------------------------------------------------
// Provider & convenience consumer hook
// ---------------------------------------------------------------------------

export function AppStateProvider({ children }: { children: ReactNode }) {
  const state = useAppStateCore();
  return createElement(AppStateContext.Provider, { value: state }, children);
}

export function useAppState(): AppState {
  const ctx = useContext(AppStateContext);
  if (ctx === null) {
    throw new Error("useAppState must be used within an <AppStateProvider>");
  }
  return ctx;
}
