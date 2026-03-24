import * as React from "react";
import { cn } from "@/lib/utils";

// ---------------------------------------------------------------------------
// ListRow - A unified row component for all list views
// ---------------------------------------------------------------------------

export interface ListRowProps extends React.HTMLAttributes<HTMLDivElement> {
  /** Whether this row is currently selected */
  selected?: boolean;
  /** Whether this row is clickable (adds hover styles and cursor) */
  clickable?: boolean;
  /** Called when the row is clicked */
  onRowClick?: () => void;
  /** Optional element to render as the row (default: div, can be button) */
  as?: "div" | "button";
}

const ListRow = React.forwardRef<HTMLDivElement, ListRowProps>(
  (
    {
      className,
      children,
      selected = false,
      clickable = true,
      onRowClick,
      as = "div",
      ...props
    },
    ref
  ) => {
    const Component = as as React.ElementType;
    return (
      <Component
        ref={ref}
        onClick={onRowClick}
        className={cn(
          "flex w-full items-center gap-3 px-4 py-2 text-left transition-colors border-b border-border last:border-b-0",
          clickable && "hover:bg-accent/50 cursor-pointer",
          selected && "bg-accent/50",
          className
        )}
        {...props}
      >
        {children}
      </Component>
    );
  }
);
ListRow.displayName = "ListRow";

// ---------------------------------------------------------------------------
// Cell Components - Building blocks for rows
// ---------------------------------------------------------------------------

/** Cell for icons (leading icon in a row) */
interface IconCellProps {
  children: React.ReactNode;
  className?: string;
}

function IconCell({ children, className }: IconCellProps) {
  return <div className={cn("shrink-0", className)}>{children}</div>;
}

/** Cell for IDs (monospace, muted text) */
interface IdCellProps {
  children: React.ReactNode;
  className?: string;
  /** Width class (default: w-16) */
  width?: string;
}

function IdCell({ children, className, width = "w-16" }: IdCellProps) {
  return (
    <span
      className={cn(
        "shrink-0 font-mono text-xs text-muted-foreground",
        width,
        className
      )}
    >
      {children}
    </span>
  );
}

/** Cell for main text content (truncates, fills available space) */
interface TextCellProps {
  children: React.ReactNode;
  className?: string;
  /** Whether this cell should expand to fill available space */
  flex?: boolean;
  /** Text size class */
  size?: "xs" | "sm" | "base";
  /** Whether text is muted */
  muted?: boolean;
  /** Whether text is monospace */
  mono?: boolean;
  /** Whether to truncate text */
  truncate?: boolean;
}

function TextCell({
  children,
  className,
  flex = true,
  size = "sm",
  muted = false,
  mono = false,
  truncate = true,
}: TextCellProps) {
  return (
    <span
      className={cn(
        flex && "flex-1 min-w-0",
        truncate && "truncate",
        size === "xs" && "text-xs",
        size === "sm" && "text-sm",
        size === "base" && "text-base",
        muted && "text-muted-foreground",
        mono && "font-mono",
        className
      )}
    >
      {children}
    </span>
  );
}

/** Cell for badges (fixed width, no shrink) */
interface BadgeCellProps {
  children: React.ReactNode;
  className?: string;
}

function BadgeCell({ children, className }: BadgeCellProps) {
  return <div className={cn("shrink-0", className)}>{children}</div>;
}

/** Cell for timestamps (right-aligned, muted, fixed width) */
interface TimeCellProps {
  children: React.ReactNode;
  className?: string;
  /** Width class (default: w-16) */
  width?: string;
  /** Whether to show an icon before the time */
  icon?: React.ReactNode;
}

function TimeCell({ children, className, width = "w-16", icon }: TimeCellProps) {
  if (icon) {
    return (
      <div
        className={cn(
          "flex items-center gap-1 text-xs text-muted-foreground shrink-0",
          width,
          className
        )}
      >
        {icon}
        <span>{children}</span>
      </div>
    );
  }
  return (
    <span
      className={cn(
        "shrink-0 text-right text-xs text-muted-foreground",
        width,
        className
      )}
    >
      {children}
    </span>
  );
}

/** Cell for action buttons (no shrink, stops click propagation) */
interface ActionsCellProps {
  children: React.ReactNode;
  className?: string;
}

function ActionsCell({ children, className }: ActionsCellProps) {
  return (
    <div
      className={cn("flex items-center gap-1 shrink-0", className)}
      onClick={(e) => e.stopPropagation()}
    >
      {children}
    </div>
  );
}

/** Cell for links (external links with icon) */
interface LinkCellProps {
  href: string;
  children: React.ReactNode;
  className?: string;
  icon?: React.ReactNode;
  /** Prevent row click when clicking link */
  stopPropagation?: boolean;
}

function LinkCell({
  href,
  children,
  className,
  icon,
  stopPropagation = true,
}: LinkCellProps) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className={cn(
        "inline-flex items-center gap-1 text-blue-400 hover:underline text-xs font-mono shrink-0",
        className
      )}
      onClick={stopPropagation ? (e) => e.stopPropagation() : undefined}
    >
      {children}
      {icon}
    </a>
  );
}

/** Cell for project names */
interface ProjectCellProps {
  children: React.ReactNode;
  className?: string;
}

function ProjectCell({ children, className }: ProjectCellProps) {
  return (
    <span className={cn("shrink-0 text-xs text-muted-foreground", className)}>
      {children}
    </span>
  );
}

// ---------------------------------------------------------------------------
// ListRowGroup - For collapsible grouped sections
// ---------------------------------------------------------------------------

interface ListRowGroupProps {
  /** Header content (icon, label, count) */
  header: React.ReactNode;
  /** Whether the group is expanded */
  isOpen: boolean;
  /** Called when the header is clicked */
  onToggle: () => void;
  /** Children to render when expanded */
  children: React.ReactNode;
  className?: string;
}

function ListRowGroup({
  header,
  isOpen,
  onToggle,
  children,
  className,
}: ListRowGroupProps) {
  return (
    <div className={className}>
      <button
        onClick={onToggle}
        className="flex w-full items-center gap-2 px-4 py-2 text-sm hover:bg-accent/30 transition-colors"
      >
        {header}
      </button>
      {isOpen && <div>{children}</div>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

export {
  ListRow,
  IconCell,
  IdCell,
  TextCell,
  BadgeCell,
  TimeCell,
  ActionsCell,
  LinkCell,
  ProjectCell,
  ListRowGroup,
};
