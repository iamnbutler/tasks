import * as React from "react";
import { cn } from "@/lib/utils";
import { ScrollArea } from "./scroll-area";

// ---------------------------------------------------------------------------
// ListView - Container for list views with scroll and empty state
// ---------------------------------------------------------------------------

interface ListViewProps {
  /** Header content (usually ListHeader component) */
  header?: React.ReactNode;
  /** Children (list items) */
  children: React.ReactNode;
  /** Whether the list is empty */
  isEmpty?: boolean;
  /** Empty state content */
  emptyState?: React.ReactNode;
  /** Additional class names for the container */
  className?: string;
  /** Additional class names for the scroll area */
  scrollClassName?: string;
  /** Whether to include scroll area wrapper (default: true) */
  scroll?: boolean;
}

function ListView({
  header,
  children,
  isEmpty = false,
  emptyState,
  className,
  scrollClassName,
  scroll = true,
}: ListViewProps) {
  const content = isEmpty && emptyState ? emptyState : children;

  return (
    <div className={cn("flex flex-col h-full", className)}>
      {header}
      {scroll ? (
        <ScrollArea className={cn("flex-1", scrollClassName)}>
          {content}
        </ScrollArea>
      ) : (
        <div className={cn("flex-1 overflow-auto", scrollClassName)}>
          {content}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// ListEmptyState - Standardized empty state display
// ---------------------------------------------------------------------------

interface ListEmptyStateProps {
  /** Icon to display */
  icon?: React.ReactNode;
  /** Main message */
  message: string;
  /** Optional description */
  description?: string;
  /** Optional action button */
  action?: React.ReactNode;
  /** Additional class names */
  className?: string;
}

function ListEmptyState({
  icon,
  message,
  description,
  action,
  className,
}: ListEmptyStateProps) {
  return (
    <div
      className={cn(
        "flex flex-col items-center justify-center py-20 px-4 text-center",
        className
      )}
    >
      {icon && (
        <div className="flex h-12 w-12 items-center justify-center rounded-full bg-accent mb-4">
          {icon}
        </div>
      )}
      <h3 className="text-sm font-medium mb-1">{message}</h3>
      {description && (
        <p className="text-sm text-muted-foreground max-w-sm">{description}</p>
      )}
      {action && <div className="mt-4">{action}</div>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// ListLoadingState - Standardized loading state display
// ---------------------------------------------------------------------------

interface ListLoadingStateProps {
  /** Loading message */
  message?: string;
  /** Additional class names */
  className?: string;
}

function ListLoadingState({
  message = "Loading...",
  className,
}: ListLoadingStateProps) {
  return (
    <div
      className={cn(
        "flex items-center justify-center h-full py-32",
        className
      )}
    >
      <p className="text-muted-foreground text-sm">{message}</p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ListErrorState - Standardized error state display
// ---------------------------------------------------------------------------

interface ListErrorStateProps {
  /** Error message */
  message: string;
  /** Optional retry action */
  onRetry?: () => void;
  /** Additional class names */
  className?: string;
}

function ListErrorState({
  message,
  onRetry,
  className,
}: ListErrorStateProps) {
  return (
    <div
      className={cn(
        "flex flex-col items-center justify-center py-20 gap-3",
        className
      )}
    >
      <p className="text-red-400 text-sm">{message}</p>
      {onRetry && (
        <button
          onClick={onRetry}
          className="text-sm text-muted-foreground hover:text-foreground underline"
        >
          Try again
        </button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// ListSplitView - For list + detail panel layouts
// ---------------------------------------------------------------------------

interface ListSplitViewProps {
  /** Main list content */
  children: React.ReactNode;
  /** Side panel content (if shown) */
  panel?: React.ReactNode;
  /** Width of the panel (default: w-96) */
  panelWidth?: string;
  /** Additional class names */
  className?: string;
}

function ListSplitView({
  children,
  panel,
  panelWidth = "w-96",
  className,
}: ListSplitViewProps) {
  return (
    <div className={cn("flex h-full", className)}>
      <div
        className={cn(
          "flex flex-col h-full transition-all",
          panel ? "flex-1" : "w-full"
        )}
      >
        {children}
      </div>
      {panel && (
        <div className={cn("shrink-0", panelWidth)}>{panel}</div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

export {
  ListView,
  ListEmptyState,
  ListLoadingState,
  ListErrorState,
  ListSplitView,
};
export type {
  ListViewProps,
  ListEmptyStateProps,
  ListLoadingStateProps,
  ListErrorStateProps,
  ListSplitViewProps,
};
