import type { Subprocess } from "bun";
import { encode, LineReader } from "../protocol/codec";
import type { SupervisorCommand } from "../protocol/commands";
import type { SupervisorEvent } from "../protocol/events";

/** Transport abstraction for host↔supervisor communication. */
export interface SessionTransport {
  send(command: SupervisorCommand): void;
  onEvent(cb: (event: SupervisorEvent) => void): void;
  onClose(cb: (reason: string) => void): void;
  close(): Promise<void>;
}

/**
 * Stdio-based transport. Wraps a child process (the `container run` process)
 * and speaks the JSON-line protocol over its stdin/stdout.
 */
export class StdioTransport implements SessionTransport {
  private proc: Subprocess;
  private eventCallbacks: Array<(event: SupervisorEvent) => void> = [];
  private closeCallbacks: Array<(reason: string) => void> = [];
  private reader: LineReader;
  private closed = false;

  constructor(proc: Subprocess) {
    this.proc = proc;

    this.reader = new LineReader((msg) => {
      // Only forward events (messages with "ev" field)
      if ("ev" in msg) {
        for (const cb of this.eventCallbacks) {
          cb(msg as SupervisorEvent);
        }
      }
    });

    this.pipeStdout();

    // Monitor process exit
    proc.exited.then((code) => {
      this.closed = true;
      const reason = `Process exited with code ${code}`;
      for (const cb of this.closeCallbacks) {
        cb(reason);
      }
    });
  }

  send(command: SupervisorCommand): void {
    const stdin = this.proc.stdin;
    if (this.closed || !stdin || typeof stdin === "number") return;
    stdin.write(encode(command));
    stdin.flush();
  }

  onEvent(cb: (event: SupervisorEvent) => void): void {
    this.eventCallbacks.push(cb);
  }

  onClose(cb: (reason: string) => void): void {
    this.closeCallbacks.push(cb);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.proc.kill("SIGTERM");
    await this.proc.exited;
  }

  private async pipeStdout(): Promise<void> {
    const stdout = this.proc.stdout;
    if (!stdout || typeof stdout === "number") return;
    const reader = stdout.getReader();
    const decoder = new TextDecoder();
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        this.reader.push(decoder.decode(value, { stream: true }));
      }
    } catch {
      // Stream closed
    }
    this.reader.flush();
  }
}
