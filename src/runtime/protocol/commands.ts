/** Commands sent from host to container supervisor over stdin. */

export interface StartCommand {
  cmd: "start";
  repo: string;
  branch: string;
  prompt: string;
}

export interface ChatCommand {
  cmd: "chat";
  text: string;
}

export interface StopCommand {
  cmd: "stop";
}

export interface ExecCommand {
  cmd: "exec";
  id: string;
  argv: string[];
}

/** Discriminated union of all host→container commands. */
export type SupervisorCommand =
  | StartCommand
  | ChatCommand
  | StopCommand
  | ExecCommand;
