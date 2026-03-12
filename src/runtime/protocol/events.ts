/** Events emitted from container supervisor to host over stdout. */

export interface SystemReadyEvent {
  ev: "system:ready";
}

export interface AgentStartedEvent {
  ev: "agent:started";
  pid: number;
}

export interface AgentStdoutEvent {
  ev: "agent:stdout";
  data: string;
}

export interface AgentStderrEvent {
  ev: "agent:stderr";
  data: string;
}

export interface AgentExitEvent {
  ev: "agent:exit";
  code: number | null;
  signal: string | null;
}

export interface ExecResultEvent {
  ev: "exec:result";
  id: string;
  code: number;
  stdout: string;
  stderr: string;
}

/** Discriminated union of all container→host events. */
export type SupervisorEvent =
  | SystemReadyEvent
  | AgentStartedEvent
  | AgentStdoutEvent
  | AgentStderrEvent
  | AgentExitEvent
  | ExecResultEvent;
