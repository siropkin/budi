import * as React from "react";
import * as RechartsPrimitive from "recharts";
import { cn } from "@/lib/utils";

export type ChartConfig = Record<
  string,
  {
    label?: React.ReactNode;
    color?: string;
  }
>;

const ChartContext = React.createContext<{ config: ChartConfig } | null>(null);

export function useChart() {
  const context = React.useContext(ChartContext);
  if (!context) {
    throw new Error("useChart must be used within a ChartContainer");
  }
  return context;
}

export function ChartContainer({
  id,
  className,
  children,
  config,
}: React.ComponentProps<"div"> & {
  config: ChartConfig;
  children: React.ComponentProps<typeof RechartsPrimitive.ResponsiveContainer>["children"];
}) {
  const chartId = React.useId();
  const identifier = `chart-${id || chartId.replace(/:/g, "")}`;

  return (
    <ChartContext.Provider value={{ config }}>
      <div
        data-chart={identifier}
        className={cn("h-[280px] w-full text-xs [&_.recharts-cartesian-axis-tick_text]:fill-muted-foreground", className)}
      >
        <ChartStyle id={identifier} config={config} />
        <RechartsPrimitive.ResponsiveContainer>{children}</RechartsPrimitive.ResponsiveContainer>
      </div>
    </ChartContext.Provider>
  );
}

function ChartStyle({ id, config }: { id: string; config: ChartConfig }) {
  const colorConfig = Object.entries(config).filter(([, value]) => value.color);
  if (colorConfig.length === 0) return null;

  return (
    <style
      dangerouslySetInnerHTML={{
        __html: `[data-chart=${id}] {${colorConfig
          .map(([key, value]) => `--color-${key}: ${value.color};`)
          .join("")}}`,
      }}
    />
  );
}

export const ChartTooltip = RechartsPrimitive.Tooltip;
