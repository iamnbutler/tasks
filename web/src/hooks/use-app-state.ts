import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";
import { createElement } from "react";
import type { Event, Snapshot } from "@/lib/types";
import { fetchSnapshot, subscribeEvents } from "@/lib/api";

const MAX_EVENTS = 200;
const POLL_INTERVAL_MS = 5_000;

/** Regex matching event types that should trigger a snapshot refresh. */
const STATE_CHANGING_EVENT = /^(task:|merge:|system:mode)/;

export interface AppState {
  snapshot: Snapshot | null;
  events: Event[];
  connected: boolean;
  error: Error | null;
  refreshSnapshot: () => Promise<void>;
}

const defaultState: AppState = {
  snapshot: null,
  events: [],
  connected: false,
  error: null,
  refreshSnapshot: async () => {},
};

export const AppStateContext = createContext<AppState | null>(null);

// ---------------------------------------------------------------------------
// Core hook – manages polling, SSE, events list
// ---------------------------------------------------------------------------

function useAppStateCore(): AppState {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [events, setEvents] = useState<Event[]>([]);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<Error | null>(null);

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

  // --- Polling -----------------------------------------------------------
  useEffect(() => {
    // Fetch immediately on mount.
    refreshSnapshot();

    const interval = setInterval(() => {
      refreshSnapshot();
    }, POLL_INTERVAL_MS);

    return () => clearInterval(interval);
  }, [refreshSnapshot]);

  // --- SSE subscription --------------------------------------------------
  useEffect(() => {
    const source = subscribeEvents();

    source.onopen = () => {
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

        // Auto-refresh snapshot when a state-changing event arrives.
        if (STATE_CHANGING_EVENT.test(event.type)) {
          refreshSnapshot();
        }
      } catch {
        // Ignore unparseable messages.
      }
    };

    source.onerror = () => {
      setConnected(false);
      setError(new Error("SSE connection lost"));
    };

    return () => {
      source.close();
      setConnected(false);
    };
  }, [refreshSnapshot]);

  return { snapshot, events, connected, error, refreshSnapshot };
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
