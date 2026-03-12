import type { SupervisorCommand } from "./commands";
import type { SupervisorEvent } from "./events";

export type Message = SupervisorCommand | SupervisorEvent;

/** Serialize a message to a JSON line (with trailing newline). */
export function encode(msg: Message): string {
  return JSON.stringify(msg) + "\n";
}

/** Parse a single JSON line into a message. Returns null on invalid input. */
export function decodeLine(line: string): Message | null {
  const trimmed = line.trim();
  if (!trimmed) return null;
  try {
    return JSON.parse(trimmed) as Message;
  } catch {
    return null;
  }
}

/**
 * Line-buffered stream reader. Buffers partial lines and yields complete
 * messages as they arrive.
 */
export class LineReader {
  private buffer = "";
  private callback: (msg: Message) => void;

  constructor(callback: (msg: Message) => void) {
    this.callback = callback;
  }

  /** Feed raw data from a stream. Complete lines are decoded and dispatched. */
  push(chunk: string): void {
    this.buffer += chunk;
    const lines = this.buffer.split("\n");
    // Last element is either empty (if chunk ended with \n) or a partial line
    this.buffer = lines.pop()!;
    for (const line of lines) {
      const msg = decodeLine(line);
      if (msg) this.callback(msg);
    }
  }

  /** Flush any remaining data in the buffer. */
  flush(): void {
    if (this.buffer.trim()) {
      const msg = decodeLine(this.buffer);
      if (msg) this.callback(msg);
    }
    this.buffer = "";
  }
}
