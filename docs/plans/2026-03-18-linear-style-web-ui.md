# Linear-Style Web UI Redesign

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Redesign the web frontend to match Linear's layout patterns — grouped sidebar navigation, grouped task lists, properties sidebar on detail pages.

**Architecture:** Keep existing React + shadcn/ui + TanStack Table stack. Replace the flat table task view with a grouped list. Add a properties sidebar to task detail. Restyle the sidebar with collapsible sections and inline project list.

**Tech Stack:** React, shadcn/ui (tabs, scroll-area, collapsible, breadcrumb, tooltip), Tailwind CSS v4, TanStack Table, lucide-react

**GitHub Issues:** #217 (sidebar), #218 (tasks list), #219 (task detail), #220 (page polish)

---

## Task 1: Redesign sidebar layout (#217)

**Files:**
- Modify: `web/src/components/layout.tsx`
- Modify: `web/src/components/project-switcher.tsx` (remove or simplify)
- Modify: `web/src/components/mode-control.tsx`
- Modify: `web/src/App.tsx` (add TooltipProvider)

**Changes:**
- Sidebar wider (w-56 / 224px) to match Linear proportions
- Header: "Tasks" with mode indicator dot (not 3 buttons)
- Mode control: single button that cycles or shows current state with colored dot
- Navigation grouped into sections:
  - Main: Tasks, Merge Queue
  - System: Orchestrator, Events
- Projects section: list tracked projects inline (click to filter), collapsible
- Add project button at bottom of projects section
- Connection status as small indicator in footer
- Wrap App with TooltipProvider for tooltips

## Task 2: Redesign tasks list page (#218)

**Files:**
- Rewrite: `web/src/pages/tasks/page.tsx`
- Delete or simplify: `web/src/pages/tasks/data-table.tsx`, `data-table-toolbar.tsx`, `data-table-pagination.tsx`, `data-table-faceted-filter.tsx`, `data-table-column-header.tsx`
- Keep: `web/src/pages/tasks/columns.tsx` (for state metadata, priority config)

**Changes:**
- Page header with "Tasks" title
- Tab bar: All | Active | Completed (using shadcn Tabs)
  - Active = running, question, testing, awaiting_merge, conflict, waiting, blocked
  - Completed = completed, failed, cancelled
- Tasks grouped by state within tab, collapsible groups with count
- Linear-style task row: priority dot, state icon, title, project badge, updated time
- Click row navigates to /tasks/:id
- Search input in header area
- Remove checkboxes, pagination (use scroll instead), faceted filters

## Task 3: Redesign task detail page (#219)

**Files:**
- Rewrite: `web/src/pages/task-detail.tsx`

**Changes:**
- Two-column layout: main content (flex-1) + properties sidebar (w-72)
- Breadcrumb: Tasks > Task title (using shadcn Breadcrumb)
- Properties sidebar: State, Priority, Project, Labels, Source (GitHub link), Session, Created, Updated, Retries, Blocked by
- Main content: Session view (full height minus header)
- Chat input at bottom of session view
- Failure info as collapsible section in sidebar
- Description as collapsible section below session
- Event timeline hidden by default, available as tab or collapsible

## Task 4: Polish remaining pages (#220)

**Files:**
- Modify: `web/src/pages/merge-queue/page.tsx`
- Modify: `web/src/pages/orchestrator/page.tsx`
- Modify: `web/src/pages/events/page.tsx`

**Changes:**
- Consistent page title styling
- Remove excess Card wrappers where unnecessary
- Consistent spacing and border treatment
- Events page: use shadcn Tabs for filter categories instead of button row
