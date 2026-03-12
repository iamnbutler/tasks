import type { Subprocess } from "bun";
import type { SupervisorEvent } from "../protocol/events";

const KILL_TIMEOUT_MS = 5_000;

export class AgentManager {
  private proc: Subprocess | null = null;
  private pendingChat: string[] = [];
  private emit: (event: SupervisorEvent) => void;

  constructor(emit: (event: SupervisorEvent) => void) {
    this.emit = emit;
  }

  get running(): boolean {
    return this.proc !== null;
  }

  /** Spawn the agent CLI as a child process. */
  async startAgent(workDir: string, prompt: string): Promise<void> {
    // Agent command is configurable via env, defaulting to a simple echo for testing
    const agentCmd = process.env.AGENT_CMD ?? "claude";
    const agentArgs = process.env.AGENT_ARGS?.split(" ") ?? [
      "--print",
      "--output-format",
      "stream-json",
    ];

    this.proc = Bun.spawn([agentCmd, ...agentArgs, prompt], {
      cwd: workDir,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
      env: { ...process.env },
    });

    this.emit({ ev: "agent:started", pid: this.proc.pid });

    // Pipe stdout
    if (this.proc.stdout && typeof this.proc.stdout !== "number") {
      this.pipeStream(this.proc.stdout, (data) =>
        this.emit({ ev: "agent:stdout", data })
      );
    }

    // Pipe stderr
    if (this.proc.stderr && typeof this.proc.stderr !== "number") {
      this.pipeStream(this.proc.stderr, (data) =>
        this.emit({ ev: "agent:stderr", data })
      );
    }

    // Handle exit
    this.proc.exited.then((code) => {
      const signal = this.proc?.signalCode ?? null;
      this.proc = null;
      this.emit({ ev: "agent:exit", code, signal: signal ?? null });
    });

    // Flush any pending chat messages
    for (const text of this.pendingChat) {
      this.writeToStdin(text);
    }
    this.pendingChat = [];
  }

  /** Send a chat message to the agent's stdin. Buffers if agent isn't running. */
  sendChat(text: string): void {
    if (!this.proc) {
      this.pendingChat.push(text);
      return;
    }
    this.writeToStdin(text);
  }

  /** Gracefully stop the agent: SIGTERM → wait → SIGKILL. */
  async stopAgent(): Promise<void> {
    if (!this.proc) return;

    const proc = this.proc;
    proc.kill("SIGTERM");

    const exited = await Promise.race([
      proc.exited,
      new Promise<"timeout">((resolve) =>
        setTimeout(() => resolve("timeout"), KILL_TIMEOUT_MS)
      ),
    ]);

    if (exited === "timeout" && this.proc === proc) {
      proc.kill("SIGKILL");
      await proc.exited;
    }
  }

  private writeToStdin(text: string): void {
    const stdin = this.proc?.stdin;
    if (!stdin || typeof stdin === "number") return;
    stdin.write(text + "\n");
    stdin.flush();
  }

  private async pipeStream(
    stream: ReadableStream<Uint8Array> | null,
    onData: (data: string) => void
  ): Promise<void> {
    if (!stream) return;
    const reader = stream.getReader();
    const decoder = new TextDecoder();
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        onData(decoder.decode(value, { stream: true }));
      }
    } catch {
      // Stream closed
    }
  }
}
