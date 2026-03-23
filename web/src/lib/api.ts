import type {
  Event,
  MergeQueueEntry,
  Mode,
  Project,
  Snapshot,
  Task,
} from "./types";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, init);
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${path}`);
  }
  return res.json();
}

async function requestVoid(path: string, init?: RequestInit): Promise<void> {
  const res = await fetch(path, init);
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${path}`);
  }
}

export function fetchSnapshot(): Promise<Snapshot> {
  return request<Snapshot>("/api/snapshot");
}

export function fetchTasks(): Promise<Task[]> {
  return request<Task[]>("/api/tasks");
}

export function fetchTask(id: string): Promise<Task> {
  return request<Task>(`/api/tasks/${id}`);
}

export function fetchTaskEvents(id: string): Promise<Event[]> {
  return request<Event[]>(`/api/tasks/${id}/events`);
}

export function fetchProjects(): Promise<Project[]> {
  return request<Project[]>("/api/projects");
}

export function fetchMergeQueue(): Promise<MergeQueueEntry[]> {
  return request<MergeQueueEntry[]>("/api/merge-queue");
}

export function fetchMode(): Promise<{ mode: Mode }> {
  return request<{ mode: Mode }>("/api/mode");
}

export function setMode(mode: Mode): Promise<{ mode: Mode }> {
  return request<{ mode: Mode }>("/api/mode", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ mode }),
  });
}

export function approveMerge(id: string): Promise<void> {
  return requestVoid(`/api/merge-queue/${id}/approve`, { method: "POST" });
}

export function rejectMerge(id: string): Promise<void> {
  return requestVoid(`/api/merge-queue/${id}/reject`, { method: "POST" });
}

export function requestChanges(
  id: string,
  reasoning: string,
  feedback: string
): Promise<void> {
  return requestVoid(`/api/merge-queue/${id}/request-changes`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ reasoning, feedback }),
  });
}

export function flushMergeQueue(): Promise<string[]> {
  return request<string[]>("/api/merge-queue/flush", { method: "POST" });
}

export function addProject(repo: string): Promise<Project> {
  return request<Project>("/api/projects", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ repo }),
  });
}

export function deleteProject(id: string): Promise<void> {
  return requestVoid(`/api/projects/${id}`, { method: "DELETE" });
}

export function sendChat(taskId: string, message: string): Promise<void> {
  return requestVoid(`/api/tasks/${taskId}/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message }),
  });
}

export function cancelTask(taskId: string): Promise<void> {
  return requestVoid(`/api/tasks/${taskId}/cancel`, { method: "POST" });
}

export function sendOrchestratorChat(message: string): Promise<void> {
  return requestVoid("/api/orchestrator/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message }),
  });
}

export interface CreateIssueRequest {
  project_id: string;
  title: string;
  body?: string;
  labels?: string[];
}

export interface CreateIssueResponse {
  number: number;
  url: string;
}

export function createIssue(req: CreateIssueRequest): Promise<CreateIssueResponse> {
  return request<CreateIssueResponse>("/api/issues", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

export function subscribeEvents(opts?: {
  pattern?: string;
  task_id?: string;
}): EventSource {
  const params = new URLSearchParams();
  if (opts?.pattern) params.set("pattern", opts.pattern);
  if (opts?.task_id) params.set("task_id", opts.task_id);
  const query = params.toString();
  const url = query ? `/api/events?${query}` : "/api/events";
  return new EventSource(url);
}

// Completions API (Haiku-powered fast LLM tasks)

export interface CompletionRequest {
  prompt: string;
  system?: string;
  max_tokens?: number;
}

export interface CompletionResponse {
  text: string;
}

export function complete(req: CompletionRequest): Promise<CompletionResponse> {
  return request<CompletionResponse>("/api/completions", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
}

export function summarize(
  text: string,
  maxWords?: number
): Promise<{ summary: string }> {
  return request<{ summary: string }>("/api/completions/summarize", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ text, max_words: maxWords }),
  });
}
