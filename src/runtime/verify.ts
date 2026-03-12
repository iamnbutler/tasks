/**
 * Verification script for the session runtime stack.
 *
 * Usage:
 *   bun run src/runtime/verify.ts              # test through a real container
 *   bun run src/runtime/verify.ts --local      # test supervisor directly (no container)
 */

import { encode, LineReader } from "./protocol/codec";
import type { SupervisorEvent } from "./protocol/events";

const useContainer = !process.argv.includes("--local");

const events: SupervisorEvent[] = [];
let readyResolve: (() => void) | null = null;
let exitResolve: (() => void) | null = null;
let execResolve: (() => void) | null = null;

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
    if (event.ev === "exec:result" && execResolve) {
      execResolve();
      execResolve = null;
    }
  }
});

// --- Spawn the process ---

let proc: ReturnType<typeof Bun.spawn>;

if (useContainer) {
  console.log("Mode: container (tasks-session image)");
  console.log("Starting container...\n");

  proc = Bun.spawn(
    ["container", "run", "-i", "--rm", "-e", "AGENT_CMD=echo", "-e", "AGENT_ARGS=", "tasks-session"],
    { stdin: "pipe", stdout: "pipe", stderr: "inherit" }
  );
} else {
  // Local mode: spawn supervisor directly with a temp workspace
  const tmpDir = (await import("os")).tmpdir();
  const workspaceDir = `${tmpDir}/tasks-verify-${Date.now()}`;
  await import("fs/promises").then((fs) => fs.mkdir(workspaceDir, { recursive: true }));
  const { execSync } = await import("child_process");
  execSync("git init", { cwd: workspaceDir, stdio: "ignore" });

  console.log(`Mode: local (supervisor direct)`);
  console.log(`Workspace: ${workspaceDir}\n`);

  proc = Bun.spawn(["bun", "run", import.meta.dir + "/supervisor/main.ts"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: {
      ...process.env,
      WORKSPACE_DIR: workspaceDir,
      AGENT_CMD: "/bin/echo",
      AGENT_ARGS: "",
    },
  });
}

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

function waitFor(
  check: () => boolean,
  assignResolve: (resolve: () => void) => void,
  timeoutMs = 10_000
): Promise<boolean> {
  if (check()) return Promise.resolve(true);
  return new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => resolve(false), timeoutMs);
    assignResolve(() => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

// --- Test sequence ---

let passed = 0;
let failed = 0;

function check(name: string, ok: boolean): void {
  if (ok) {
    console.log(`   ✓ ${name}`);
    passed++;
  } else {
    console.log(`   ✗ ${name}`);
    failed++;
  }
}

console.log("1. Waiting for system:ready...");
const ready = await waitFor(
  () => events.some((e) => e.ev === "system:ready"),
  (r) => { readyResolve = r; }
);
check("system:ready", ready);

console.log("\n2. Sending exec command...");
send({ cmd: "exec", id: "test-1", argv: ["echo", "hello from exec"] });
const gotExec = await waitFor(
  () => events.some((e) => e.ev === "exec:result" && e.id === "test-1"),
  (r) => { execResolve = r; }
);
const execResult = events.find((e) => e.ev === "exec:result" && e.id === "test-1");
check("exec:result received", gotExec);
if (execResult && execResult.ev === "exec:result") {
  check("exec stdout correct", execResult.stdout.trim() === "hello from exec");
  check("exec code 0", execResult.code === 0);
}

// Seed /workspace with a git repo so the supervisor skips cloning
console.log("\n3. Preparing workspace...");
send({ cmd: "exec", id: "setup-1", argv: ["git", "init", "/workspace"] });
await waitFor(
  () => events.some((e) => e.ev === "exec:result" && e.id === "setup-1"),
  (r) => { execResolve = r; }
);

console.log("\n4. Sending start command (mock agent: echo)...");
send({
  cmd: "start",
  repo: "https://github.com/test/test.git",
  branch: "test-branch",
  prompt: "Hello from the verification script!",
});

const gotExit = await waitFor(
  () => events.some((e) => e.ev === "agent:exit"),
  (r) => { exitResolve = r; }
);

check("agent:started", events.some((e) => e.ev === "agent:started"));
check("agent:stdout", events.some((e) => e.ev === "agent:stdout"));
check("agent:exit", gotExit);

const agentExit = events.find((e) => e.ev === "agent:exit");
if (agentExit && agentExit.ev === "agent:exit") {
  check("agent exit code 0", agentExit.code === 0);
}

console.log("\n5. Stopping...");
const stdin = proc.stdin;
if (stdin && typeof stdin !== "number") {
  stdin.end();
}
proc.kill("SIGTERM");
const exitCode = await Promise.race([
  proc.exited,
  new Promise<"timeout">((r) => setTimeout(() => r("timeout"), 5_000)),
]);
if (exitCode === "timeout") {
  proc.kill("SIGKILL");
  await proc.exited;
}
check("clean shutdown", true);

console.log(`\nResults: ${passed} passed, ${failed} failed`);
process.exit(failed > 0 ? 1 : 0);
