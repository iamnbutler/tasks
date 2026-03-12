import type {
  ContainerRuntime,
  ContainerConfig,
} from "./container";
import type { SessionTransport } from "./transport";
import type { SupervisorEvent } from "../protocol/events";

export interface SessionWrapperConfig {
  containerConfig: ContainerConfig;
  onEvent?: (event: SupervisorEvent) => void;
}

/**
 * Session wrapper — the host-side manager for a single session.
 *
 * Owns a container runtime and transport. Provides a high-level API
 * for creating sessions, starting agents, sending chat messages, etc.
 * This is what the server's session manager instantiates per task.
 */
export class SessionWrapper {
  private runtime: ContainerRuntime;
  private config: SessionWrapperConfig;
  private containerId: string | null = null;
  private transport: SessionTransport | null = null;
  private readyResolve: (() => void) | null = null;

  constructor(runtime: ContainerRuntime, config: SessionWrapperConfig) {
    this.runtime = runtime;
    this.config = config;
  }

  /** Create and start the container, connect transport, wait for system:ready. */
  async create(): Promise<void> {
    this.containerId = await this.runtime.create(this.config.containerConfig);
    await this.runtime.start(this.containerId);
    this.transport = this.runtime.attach(this.containerId);

    // Wire up event forwarding
    this.transport.onEvent((event) => {
      if (event.ev === "system:ready" && this.readyResolve) {
        this.readyResolve();
        this.readyResolve = null;
      }
      this.config.onEvent?.(event);
    });

    this.transport.onClose((reason) => {
      this.transport = null;
    });

    // Wait for the supervisor to be ready
    await new Promise<void>((resolve) => {
      this.readyResolve = resolve;
    });
  }

  /** Send a start command to begin agent work. */
  startAgent(repo: string, branch: string, prompt: string): void {
    this.requireTransport().send({
      cmd: "start",
      repo,
      branch,
      prompt,
    });
  }

  /** Send a chat message to the running agent. */
  sendChat(text: string): void {
    this.requireTransport().send({ cmd: "chat", text });
  }

  /** Stop the agent process (container stays running). */
  stop(): void {
    this.requireTransport().send({ cmd: "stop" });
  }

  /** Destroy the container and clean up. */
  async destroy(): Promise<void> {
    if (this.transport) {
      await this.transport.close();
      this.transport = null;
    }
    if (this.containerId) {
      await this.runtime.destroy(this.containerId);
      this.containerId = null;
    }
  }

  private requireTransport(): SessionTransport {
    if (!this.transport) {
      throw new Error("Session transport not connected");
    }
    return this.transport;
  }
}
