import { useMemo, useState } from "react";
import { ExternalLink } from "lucide-react";
import { useAppState } from "@/hooks/use-app-state";
import { formatRelativeTime, projectLabel } from "@/lib/utils";
import { ListView, ListEmptyState } from "@/components/ui/list-view";
import { ListHeader, ListHeaderTabs } from "@/components/ui/list-header";
import {
  ListRow,
  LinkCell,
  TextCell,
  BadgeCell,
  TimeCell,
  ProjectCell,
} from "@/components/ui/list-row";
import { Badge } from "@/components/ui/badge";
import type { Reflection, ReflectionState } from "@/lib/types";

// ---------------------------------------------------------------------------
// Tab configuration
// ---------------------------------------------------------------------------

type ReflectionTab = "open" | "closed" | "all";

const tabConfig: { key: ReflectionTab; label: string }[] = [
  { key: "open", label: "Open" },
  { key: "closed", label: "Closed" },
  { key: "all", label: "All" },
];

function stateBadge(state: ReflectionState) {
  switch (state) {
    case "open":
      return <Badge variant="default">Open</Badge>;
    case "closed":
      return <Badge variant="secondary">Closed</Badge>;
  }
}

// ---------------------------------------------------------------------------
// Reflection Row
// ---------------------------------------------------------------------------

function ReflectionRow({
  reflection,
  showProject,
  projects,
}: {
  reflection: Reflection;
  showProject: boolean;
  projects: { id: string; repo: string }[];
}) {
  const shortProject = showProject
    ? projectLabel(reflection.project, projects).split("/").pop() ?? ""
    : "";

  return (
    <ListRow>
      {/* Issue number */}
      <LinkCell
        href={reflection.url}
        icon={<ExternalLink className="h-3 w-3" />}
        className="w-16"
      >
        #{reflection.number}
      </LinkCell>

      {/* Title */}
      <TextCell>
        <a
          href={reflection.url}
          target="_blank"
          rel="noopener noreferrer"
          className="hover:underline truncate block"
        >
          {reflection.title}
        </a>
        {reflection.comments.length > 0 && (
          <span className="text-xs text-muted-foreground ml-1">
            ({reflection.comments.length} comment
            {reflection.comments.length !== 1 ? "s" : ""})
          </span>
        )}
      </TextCell>

      {/* Project */}
      {showProject && <ProjectCell>{shortProject}</ProjectCell>}

      {/* State */}
      <BadgeCell>{stateBadge(reflection.state)}</BadgeCell>

      {/* Updated */}
      <TimeCell width="w-24">{formatRelativeTime(reflection.updated_at)}</TimeCell>
    </ListRow>
  );
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function ReflectionsPage() {
  const { snapshot, selectedProject } = useAppState();
  const [activeTab, setActiveTab] = useState<ReflectionTab>("open");

  const reflections: Reflection[] = useMemo(() => {
    const all = snapshot?.reflections ?? [];
    if (!selectedProject) return all;
    return all.filter((r) => r.project === selectedProject);
  }, [snapshot, selectedProject]);

  const grouped = useMemo(() => {
    const open: Reflection[] = [];
    const closed: Reflection[] = [];
    for (const r of reflections) {
      if (r.state === "open") open.push(r);
      else closed.push(r);
    }
    return { open, closed, all: reflections };
  }, [reflections]);

  const tabs = tabConfig.map((t) => ({
    ...t,
    count: grouped[t.key].length,
  }));

  const projects = snapshot?.projects ?? [];
  const showProject = !selectedProject;
  const displayed = grouped[activeTab];

  if (!snapshot) {
    return <ListEmptyState message="Loading..." />;
  }

  return (
    <ListView
      header={
        <div className="border-b border-border">
          <ListHeader
            title="Reflections"
            count={reflections.length}
            countLabel="reflections"
          />
          <div className="px-4 pb-1">
            <ListHeaderTabs
              tabs={tabs}
              activeTab={activeTab}
              onTabChange={setActiveTab}
              variant="line"
              className="mt-1"
            />
          </div>
        </div>
      }
    >
      {displayed.length === 0 ? (
        <ListEmptyState
          message={
            activeTab === "open"
              ? "No open reflections. Create a GitHub issue with the 'reflection' label to add feedback."
              : "No reflections in this view."
          }
        />
      ) : (
        <div>
          {displayed.map((r) => (
            <ReflectionRow
              key={r.id}
              reflection={r}
              showProject={showProject}
              projects={projects}
            />
          ))}
        </div>
      )}
    </ListView>
  );
}
