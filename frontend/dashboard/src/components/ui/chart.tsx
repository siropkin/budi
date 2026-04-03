import * as React from "react";
import * as RechartsPrimitive from "recharts";
import { cn } from "@/lib/utils";

export type ChartConfig = Record<
  string,
  {
    label?: React.ReactNode;
    color?: string;
    icon?: React.ComponentType;
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
export const ChartLegend = RechartsPrimitive.Legend;

type TooltipPayloadItem = {
  color?: string;
  dataKey?: string;
  name?: string;
  value?: number | string;
  payload?: Record<string, unknown>;
};

type TooltipContentProps = React.ComponentProps<"div"> & {
  active?: boolean;
  payload?: TooltipPayloadItem[];
  label?: string | number;
  formatter?: (
    value: number | string,
    name: string,
    item: TooltipPayloadItem,
    index: number,
    payload: TooltipPayloadItem[],
  ) => React.ReactNode;
  labelFormatter?: (label: string | number, payload: TooltipPayloadItem[]) => React.ReactNode;
  hideLabel?: boolean;
  hideIndicator?: boolean;
  indicator?: "dot" | "line";
  nameKey?: string;
  labelKey?: string;
};

export function ChartTooltipContent({
  active,
  payload,
  className,
  indicator = "dot",
  hideIndicator = false,
  hideLabel = false,
  label,
  formatter,
  labelFormatter,
  nameKey,
  labelKey,
}: TooltipContentProps) {
  const { config } = useChart();

  if (!active || !payload?.length) return null;

  const labelText = (() => {
    if (hideLabel) return null;
    if (labelFormatter) return labelFormatter(label ?? "", payload);
    if (typeof label === "string" || typeof label === "number") return label;
    return null;
  })();

  return (
    <div className={cn("grid min-w-[8rem] gap-1.5 rounded-md border border-border bg-card px-3 py-2 text-xs shadow-md", className)}>
      {labelText ? <p className="font-medium text-foreground">{labelText}</p> : null}
      <div className="grid gap-1">
        {payload.map((item, index) => {
          const key = String(nameKey ? item.payload?.[nameKey] ?? item.dataKey : item.dataKey ?? item.name ?? index);
          const entry = config[key] ?? config[String(item.dataKey ?? "")];
          const displayLabel =
            entry?.label ??
            String(labelKey ? item.payload?.[labelKey] ?? item.name ?? item.dataKey ?? "value" : item.name ?? item.dataKey ?? "value");
          const value = typeof item.value === "number" ? item.value : Number(item.value ?? 0);

          if (formatter) {
            return (
              <div key={key}>
                {formatter(value, String(displayLabel), item, index, payload)}
              </div>
            );
          }

          return (
            <div key={key} className="flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                {hideIndicator ? null : (
                  <span
                    className={cn(
                      "inline-block shrink-0 rounded-sm",
                      indicator === "dot" ? "h-2 w-2 rounded-full" : "h-0.5 w-3",
                    )}
                    style={{ backgroundColor: item.color ?? `var(--color-${item.dataKey ?? ""})` }}
                  />
                )}
                <span className="text-muted-foreground">{displayLabel}</span>
              </div>
              <span className="font-medium tabular-nums text-foreground">{Number.isFinite(value) ? value.toLocaleString() : item.value}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

type LegendPayloadItem = {
  color?: string;
  dataKey?: string;
  value?: string;
  payload?: { fill?: string };
};

type LegendContentProps = React.ComponentProps<"div"> & {
  payload?: LegendPayloadItem[];
  hideIcon?: boolean;
  nameKey?: string;
};

export function ChartLegendContent({ className, payload, hideIcon = false, nameKey }: LegendContentProps) {
  const { config } = useChart();

  if (!payload?.length) return null;

  return (
    <div className={cn("flex flex-wrap items-center gap-4 pt-3 text-xs", className)}>
      {payload.map((item, index) => {
        const key = String(nameKey ? item.payload?.[nameKey as keyof typeof item.payload] ?? item.dataKey : item.dataKey ?? index);
        const entry = config[key] ?? config[String(item.dataKey ?? "")];
        const displayLabel = entry?.label ?? item.value ?? item.dataKey ?? "";
        const iconColor = item.color ?? item.payload?.fill ?? `var(--color-${item.dataKey ?? ""})`;

        return (
          <div key={key} className="flex items-center gap-1.5 text-muted-foreground">
            {hideIcon ? null : <span className="inline-block h-2 w-2 rounded-full" style={{ backgroundColor: iconColor }} />}
            <span>{displayLabel}</span>
          </div>
        );
      })}
    </div>
  );
}
