import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";
import type { Project } from "./types";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function formatRelativeTime(date: string | Date): string {
  const now = Date.now();
  const then = new Date(date).getTime();
  const diff = now - then;

  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

/**
 * Get the display name (repo) for a project ID.
 * Falls back to the raw project ID if not found.
 */
export function projectLabel(projectId: string, projects: Project[]): string {
  return projects.find((p) => p.id === projectId)?.repo ?? projectId;
}
