import * as React from "react";
import { cn } from "@/lib/utils";

interface TabsContextValue {
  value: string;
  setValue: (value: string) => void;
}

const TabsContext = React.createContext<TabsContextValue | null>(null);

export function Tabs({
  value,
  onValueChange,
  className,
  children,
}: {
  value: string;
  onValueChange: (value: string) => void;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <TabsContext.Provider value={{ value, setValue: onValueChange }}>
      <div className={className}>{children}</div>
    </TabsContext.Provider>
  );
}

export function TabsList({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("inline-flex items-center rounded-lg border border-border bg-secondary p-1 text-muted-foreground", className)}
      {...props}
    />
  );
}

export function TabsTrigger({
  value,
  className,
  children,
}: {
  value: string;
  className?: string;
  children: React.ReactNode;
}) {
  const ctx = React.useContext(TabsContext);
  if (!ctx) {
    throw new Error("TabsTrigger must be used within Tabs");
  }

  const active = ctx.value === value;

  return (
    <button
      type="button"
      className={cn(
        "rounded-md px-3 py-1.5 text-sm font-medium transition",
        active ? "bg-primary text-primary-foreground" : "hover:bg-muted hover:text-foreground",
        className,
      )}
      onClick={() => ctx.setValue(value)}
    >
      {children}
    </button>
  );
}
