import { $ } from "bun";
import { StdioTransport, type SessionTransport } from "./transport";

export interface ContainerConfig {
  image: string;
  env?: Record<string, string>;
  cpus?: number;
  memory?: string;
}

/** Abstraction for container lifecycle operations. */
export interface ContainerRuntime {
  create(config: ContainerConfig): Promise<string>;
  start(containerId: string): Promise<void>;
  stop(containerId: string): Promise<void>;
  destroy(containerId: string): Promise<void>;
  attach(containerId: string): SessionTransport;
}

/**
 * Container runtime implementation using apple/container CLI.
 *
 * Shells out to the `container` CLI for lifecycle operations and attaches
 * to the container's stdio for protocol communication.
 */
export class AppleContainerRuntime implements ContainerRuntime {
  async create(config: ContainerConfig): Promise<string> {
    const args = ["container", "create", "--image", config.image];

    if (config.env) {
      for (const [key, value] of Object.entries(config.env)) {
        args.push("--env", `${key}=${value}`);
      }
    }

    const result = await $`${args}`.text();
    return result.trim(); // container ID
  }

  async start(containerId: string): Promise<void> {
    await $`container start ${containerId}`.quiet();
  }

  async stop(containerId: string): Promise<void> {
    await $`container stop ${containerId}`.quiet();
  }

  async destroy(containerId: string): Promise<void> {
    await $`container stop ${containerId}`.quiet().nothrow();
    await $`container delete ${containerId}`.quiet();
  }

  attach(containerId: string): SessionTransport {
    const proc = Bun.spawn(["container", "attach", containerId], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    return new StdioTransport(proc);
  }
}
