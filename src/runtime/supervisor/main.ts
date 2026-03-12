import { LineReader, encode } from "../protocol/codec";
import type { SupervisorCommand } from "../protocol/commands";
import type { SupervisorEvent } from "../protocol/events";
import { AgentManager } from "./agent";
import { cloneRepo, repoExists } from "./repo";

const WORK_DIR = process.env.WORKSPACE_DIR ?? "/workspace";

/** Write a protocol event to stdout. */
function emit(event: SupervisorEvent): void {
  process.stdout.write(encode(event));
}

const agent = new AgentManager(emit);

/** Handle an incoming command from the host. */
async function handleCommand(cmd: SupervisorCommand): Promise<void> {
  switch (cmd.cmd) {
    case "start": {
      const exists = await repoExists(WORK_DIR);
      if (!exists) {
        await cloneRepo(cmd.repo, cmd.branch, WORK_DIR);
      }
      await agent.startAgent(WORK_DIR, cmd.prompt);
      break;
    }
    case "chat": {
      agent.sendChat(cmd.text);
      break;
    }
    case "stop": {
      await agent.stopAgent();
      break;
    }
    case "exec": {
      try {
        const proc = Bun.spawn(cmd.argv, {
          cwd: WORK_DIR,
          stdout: "pipe",
          stderr: "pipe",
        });
        const [stdout, stderr] = await Promise.all([
          new Response(proc.stdout).text(),
          new Response(proc.stderr).text(),
        ]);
        const code = await proc.exited;
        emit({ ev: "exec:result", id: cmd.id, code, stdout, stderr });
      } catch (err) {
        emit({
          ev: "exec:result",
          id: cmd.id,
          code: 1,
          stdout: "",
          stderr: String(err),
        });
      }
      break;
    }
  }
}

// Read commands from stdin
const reader = new LineReader((msg) => {
  // Only handle commands (messages with "cmd" field)
  if ("cmd" in msg) {
    handleCommand(msg as SupervisorCommand);
  }
});

// Graceful shutdown
process.on("SIGTERM", async () => {
  await agent.stopAgent();
  process.exit(0);
});

// Read stdin line by line
const stdinReader = process.stdin;
const decoder = new TextDecoder();

async function readStdin(): Promise<void> {
  for await (const chunk of stdinReader) {
    reader.push(decoder.decode(chunk as Buffer, { stream: true }));
  }
  reader.flush();
}

// Emit system:ready and start reading
emit({ ev: "system:ready" });
readStdin();
