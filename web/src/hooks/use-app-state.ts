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
import type { Event, Snapshot, Task, MergeQueueEntry } from "@/lib/types";
import { fetchSnapshot, subscribeEvents } from "@/lib/api";

const MAX_EVENTS = 200;
const POLL_INTERVAL_MS = 5_000;

/** Regex matching event types that should trigger a snapshot refresh. */
const STATE_CHANGING_EVENT = /^(task:|merge:|system:mode)/;

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

  // Track seen orchestrator event IDs for deduplication
  const seenOrchestratorIds = useRef(new Set<string>());

  // Keep a ref to the latest snapshot-fetch promise so we can avoid races.
  const fetchInFlight = useRef(false);

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

  // --- Polling -----------------------------------------------------------
  useEffect(() => {
    // Fetch immediately on mount.
    refreshSnapshot();

    const interval = setInterval(() => {
      refreshSnapshot();
    }, POLL_INTERVAL_MS);

    return () => clearInterval(interval);
  }, [refreshSnapshot]);

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
  }, [refreshSnapshot]);

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
