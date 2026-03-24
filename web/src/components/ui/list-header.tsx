import * as React from "react";
import { Search } from "lucide-react";
import { cn } from "@/lib/utils";
import { Input } from "./input";
import { Badge } from "./badge";
import { Tabs, TabsList, TabsTrigger } from "./tabs";

// ---------------------------------------------------------------------------
// ListHeader - Unified header for list views
// ---------------------------------------------------------------------------

interface ListHeaderProps {
  /** Page title */
  title: string;
  /** Optional item count to display */
  count?: number;
  /** Optional count label (default: "items") */
  countLabel?: string;
  /** Optional action buttons on the right */
  actions?: React.ReactNode;
  /** Optional tabs for filtering */
  tabs?: React.ReactNode;
  /** Optional search input */
  search?: {
    value: string;
    onChange: (value: string) => void;
    placeholder?: string;
  };
  /** Additional class names */
  className?: string;
}

function ListHeader({
  title,
  count,
  countLabel = "items",
  actions,
  tabs,
  search,
  className,
}: ListHeaderProps) {
  return (
    <div
      className={cn(
        "flex items-center justify-between border-b border-border px-4 py-2.5",
        className
      )}
    >
      <div className="flex items-center gap-4">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold">{title}</h1>
          {count !== undefined && (
            <span className="text-xs text-muted-foreground">
              {count} {count === 1 ? countLabel.replace(/s$/, "") : countLabel}
            </span>
          )}
        </div>
        {tabs}
      </div>

      <div className="flex items-center gap-2">
        {search && (
          <div className="relative w-48">
            <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              placeholder={search.placeholder ?? "Filter..."}
              value={search.value}
              onChange={(e) => search.onChange(e.target.value)}
              className="h-7 pl-7 text-xs"
            />
          </div>
        )}
        {actions}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ListHeaderTabs - Helper for creating standard tab navigation
// ---------------------------------------------------------------------------

interface TabConfig<T extends string> {
  key: T;
  label: string;
  count?: number;
}

interface ListHeaderTabsProps<T extends string> {
  tabs: TabConfig<T>[];
  activeTab: T;
  onTabChange: (tab: T) => void;
  /** Tab style variant */
  variant?: "default" | "line";
  className?: string;
}

function ListHeaderTabs<T extends string>({
  tabs,
  activeTab,
  onTabChange,
  variant = "default",
  className,
}: ListHeaderTabsProps<T>) {
  return (
    <Tabs
      value={activeTab}
      onValueChange={(v) => onTabChange(v as T)}
      className={className}
    >
      <TabsList className={variant === "default" ? "h-7" : undefined} variant={variant === "line" ? "line" : undefined}>
        {tabs.map(({ key, label, count }) => (
          <TabsTrigger
            key={key}
            value={key}
            className={variant === "default" ? "text-xs px-2.5 h-6" : undefined}
          >
            {label}
            {count !== undefined && count > 0 && (
              <span className="ml-1 text-muted-foreground">{count}</span>
            )}
          </TabsTrigger>
        ))}
      </TabsList>
    </Tabs>
  );
}

// ---------------------------------------------------------------------------
// ListHeaderBadge - Count badge variant for simpler displays
// ---------------------------------------------------------------------------

interface ListHeaderBadgeProps {
  count: number;
  label?: string;
  className?: string;
}

function ListHeaderBadge({ count, label, className }: ListHeaderBadgeProps) {
  return (
    <Badge variant="outline" className={cn("text-xs", className)}>
      {count} {label ?? "active"}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

export { ListHeader, ListHeaderTabs, ListHeaderBadge };
export type { ListHeaderProps, TabConfig, ListHeaderTabsProps };
