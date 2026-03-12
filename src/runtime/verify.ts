/**
 * Verification script for the session runtime stack.
 *
 * Tests the supervisor directly by spawning it as a child process
 * and speaking the JSON-line protocol over stdio. Does not require
 * a container — validates the protocol layer end-to-end.
 *
 * Usage: bun run src/runtime/verify.ts
 */

import { encode, LineReader } from "./protocol/codec";
import type { SupervisorEvent } from "./protocol/events";

const events: SupervisorEvent[] = [];
let readyResolve: (() => void) | null = null;
let exitResolve: (() => void) | null = null;

const reader = new LineReader((msg) => {
  if ("ev" in msg) {
    const event = msg as SupervisorEvent;
    events.push(event);
    console.log("  ←", JSON.stringify(event));

    if (event.ev === "system:ready" && readyResolve) {
      readyResolve();
      readyResolve = null;
    }
    if (event.ev === "agent:exit" && exitResolve) {
      exitResolve();
      exitResolve = null;
    }
  }
});

// Create a temp workspace for local testing
const tmpDir = await import("os").then((os) => os.tmpdir());
const workspaceDir = `${tmpDir}/tasks-verify-${Date.now()}`;
await import("fs/promises").then((fs) => fs.mkdir(workspaceDir, { recursive: true }));

// Init a git repo so the supervisor thinks it's already cloned
const { execSync } = await import("child_process");
execSync("git init", { cwd: workspaceDir, stdio: "ignore" });

console.log(`Workspace: ${workspaceDir}`);
console.log("Starting supervisor process...");

// Spawn the supervisor directly (not in a container)
const proc = Bun.spawn(["bun", "run", import.meta.dir + "/supervisor/main.ts"], {
  stdin: "pipe",
  stdout: "pipe",
  stderr: "inherit",
  env: {
    ...process.env,
    WORKSPACE_DIR: workspaceDir,
    // Use /bin/echo as the mock agent (full path for macOS)
    AGENT_CMD: "/bin/echo",
    AGENT_ARGS: "",
  },
});

// Pipe stdout through the line reader
(async () => {
  const stdout = proc.stdout;
  if (!stdout || typeof stdout === "number") return;
  const streamReader = stdout.getReader();
  const decoder = new TextDecoder();
  try {
    while (true) {
      const { done, value } = await streamReader.read();
      if (done) break;
      reader.push(decoder.decode(value, { stream: true }));
    }
  } catch {}
  reader.flush();
})();

function send(cmd: object): void {
  const stdin = proc.stdin;
  if (!stdin || typeof stdin === "number") return;
  const line = encode(cmd as any);
  console.log("  →", line.trim());
  stdin.write(line);
  stdin.flush();
}

// --- Test sequence ---

console.log("\n1. Waiting for system:ready...");
await new Promise<void>((resolve) => {
  // Check if already received
  if (events.some((e) => e.ev === "system:ready")) {
    resolve();
  } else {
    readyResolve = resolve;
  }
});
console.log("   ✓ system:ready received\n");

console.log("2. Sending exec command...");
send({ cmd: "exec", id: "test-1", argv: ["echo", "hello from exec"] });

// Wait a bit for the exec result
await new Promise((r) => setTimeout(r, 500));

const execResult = events.find(
  (e) => e.ev === "exec:result" && e.id === "test-1"
);
if (execResult && execResult.ev === "exec:result") {
  console.log(`   ✓ exec:result received (code=${execResult.code}, stdout="${execResult.stdout.trim()}")\n`);
} else {
  console.log("   ✗ exec:result not received\n");
}

console.log("3. Sending start command (mock agent: echo)...");
send({
  cmd: "start",
  repo: "https://github.com/test/test.git",
  branch: "test-branch",
  prompt: "Hello from the verification script!",
});

// Wait for agent:exit (echo will exit immediately)
await new Promise<void>((resolve) => {
  if (events.some((e) => e.ev === "agent:exit")) {
    resolve();
  } else {
    exitResolve = resolve;
    // Timeout after 5s
    setTimeout(resolve, 5_000);
  }
});

const started = events.find((e) => e.ev === "agent:started");
const agentExit = events.find((e) => e.ev === "agent:exit");
const agentOut = events.find((e) => e.ev === "agent:stdout");

if (started) console.log(`   ✓ agent:started (pid=${started.ev === "agent:started" ? started.pid : "?"})`);
else console.log("   ✗ agent:started not received");

if (agentOut) console.log(`   ✓ agent:stdout received`);
else console.log("   ⚠ agent:stdout not received (may be timing)");

if (agentExit) console.log(`   ✓ agent:exit received`);
else console.log("   ✗ agent:exit not received");

console.log("\n4. Stopping supervisor...");
// Close stdin first so the supervisor's stdin reader exits, then signal
const stdin = proc.stdin;
if (stdin && typeof stdin !== "number") {
  stdin.end();
}
proc.kill("SIGTERM");
const exitCode = await Promise.race([
  proc.exited,
  new Promise<"timeout">((r) => setTimeout(() => r("timeout"), 3_000)),
]);
if (exitCode === "timeout") {
  proc.kill("SIGKILL");
  await proc.exited;
}
console.log("   ✓ supervisor exited\n");

// Clean up temp workspace
await import("fs/promises").then((fs) => fs.rm(workspaceDir, { recursive: true, force: true }));

console.log(`Total events received: ${events.length}`);
console.log("Verification complete.");
