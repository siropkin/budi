import {
  Bar,
  BarChart,
  CartesianGrid,
  ComposedChart,
  LabelList,
  Line,
  XAxis,
  YAxis,
} from "recharts";
import { ChartContainer, ChartLegend, ChartLegendContent, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { EmptyState } from "@/components/state";
import { fmtCost, fmtNum } from "@/lib/format";

const MAX_BAR_ITEMS = 10;
const BAR_SIZE = 28;
const BAR_GAP = 8;
const MIN_BAR_CHART_HEIGHT = 92;
const Y_AXIS_LABEL_MAX_CHARS = 28;
const BAR_LABEL_HEADROOM_MULTIPLIER = 1.18;
const BAR_CHART_RIGHT_MARGIN = 56;

function xAxisWithHeadroom(dataMax: number): number {
  if (!Number.isFinite(dataMax) || dataMax <= 0) return 1;
  return Math.ceil(dataMax * BAR_LABEL_HEADROOM_MULTIPLIER);
}

function barChartHeight(rows: number): number {
  return Math.max(MIN_BAR_CHART_HEIGHT, rows * (BAR_SIZE + BAR_GAP) + 32);
}

function truncateLabel(value: string, maxLen = Y_AXIS_LABEL_MAX_CHARS): string {
  const normalized = value.replace(/\s+/g, " ").trim();
  if (!normalized) return "";
  if (normalized.length <= maxLen) return normalized;
  return `${normalized.slice(0, Math.max(0, maxLen - 1))}…`;
}

function YAxisTruncatedTick({
  x,
  y,
  payload,
}: {
  x?: number;
  y?: number;
  payload?: { value?: string };
}) {
  const full = String(payload?.value ?? "");
  const truncated = truncateLabel(full);

  return (
    <g transform={`translate(${x ?? 0},${y ?? 0})`}>
      <text x={0} y={0} dy={4} textAnchor="end" fill="hsl(var(--muted-foreground))" fontSize={12}>
        <title>{full}</title>
        {truncated}
      </text>
    </g>
  );
}

export interface CostBarDatum {
  label: string;
  cost_cents: number;
}

export function CostBarChart({
  data,
  emptyLabel,
}: {
  data: CostBarDatum[];
  emptyLabel: string;
}) {
  const sortedData = [...data].sort((left, right) => {
    if (right.cost_cents !== left.cost_cents) return right.cost_cents - left.cost_cents;
    return left.label.localeCompare(right.label);
  }).slice(0, MAX_BAR_ITEMS);

  if (data.length === 0) {
    return <EmptyState label={emptyLabel} />;
  }

  const chartHeight = barChartHeight(sortedData.length);

  return (
    <ChartContainer
      style={{ height: chartHeight }}
      config={{
        cost_cents: {
          label: "Cost",
          color: "hsl(var(--chart-1))",
        },
      }}
    >
      <BarChart
        data={sortedData}
        layout="vertical"
        barCategoryGap={BAR_GAP}
        margin={{ left: 20, right: BAR_CHART_RIGHT_MARGIN, top: 6, bottom: 6 }}
        accessibilityLayer
      >
        <CartesianGrid horizontal={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
        <YAxis dataKey="label" type="category" tickLine={false} axisLine={false} width={190} interval={0} tick={<YAxisTruncatedTick />} />
        <XAxis
          dataKey="cost_cents"
          type="number"
          domain={[0, xAxisWithHeadroom]}
          tickFormatter={(value) => fmtCost(value / 100)}
          axisLine={false}
          tickLine={false}
        />
        <ChartTooltip
          cursor={false}
          content={
            <ChartTooltipContent
              indicator="dot"
              formatter={(value, name) => (
                <div className="flex items-center justify-between gap-2">
                  <span className="text-muted-foreground">{name}</span>
                  <span className="font-medium tabular-nums text-foreground">{fmtCost(Number(value) / 100)}</span>
                </div>
              )}
            />
          }
        />
        <Bar dataKey="cost_cents" fill="var(--color-cost_cents)" barSize={BAR_SIZE} radius={[5, 5, 5, 5]}>
          <LabelList
            dataKey="cost_cents"
            position="right"
            className="fill-muted-foreground text-xs"
            formatter={(value: number) => fmtCost(value / 100)}
          />
        </Bar>
      </BarChart>
    </ChartContainer>
  );
}

export interface CountBarDatum {
  label: string;
  value: number;
}

export function CountBarChart({
  data,
  emptyLabel,
  valueLabel,
}: {
  data: CountBarDatum[];
  emptyLabel: string;
  valueLabel?: string;
}) {
  const sortedData = [...data].sort((left, right) => {
    if (right.value !== left.value) return right.value - left.value;
    return left.label.localeCompare(right.label);
  }).slice(0, MAX_BAR_ITEMS);

  if (data.length === 0) {
    return <EmptyState label={emptyLabel} />;
  }

  const chartHeight = barChartHeight(sortedData.length);

  return (
    <ChartContainer
      style={{ height: chartHeight }}
      config={{
        value: {
          label: valueLabel ?? "Value",
          color: "hsl(var(--chart-2))",
        },
      }}
    >
      <BarChart
        data={sortedData}
        layout="vertical"
        barCategoryGap={BAR_GAP}
        margin={{ left: 20, right: BAR_CHART_RIGHT_MARGIN, top: 6, bottom: 6 }}
        accessibilityLayer
      >
        <CartesianGrid horizontal={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
        <YAxis dataKey="label" type="category" tickLine={false} axisLine={false} width={190} interval={0} tick={<YAxisTruncatedTick />} />
        <XAxis
          dataKey="value"
          type="number"
          domain={[0, xAxisWithHeadroom]}
          axisLine={false}
          tickLine={false}
          tickFormatter={(value) => fmtNum(value)}
        />
        <ChartTooltip
          cursor={false}
          content={
            <ChartTooltipContent
              indicator="dot"
              formatter={(value, name) => (
                <div className="flex items-center justify-between gap-2">
                  <span className="text-muted-foreground">{name}</span>
                  <span className="font-medium tabular-nums text-foreground">
                    {fmtNum(Number(value))}
                    {valueLabel ? ` ${valueLabel}` : ""}
                  </span>
                </div>
              )}
            />
          }
        />
        <Bar dataKey="value" fill="var(--color-value)" barSize={BAR_SIZE} radius={[5, 5, 5, 5]}>
          <LabelList dataKey="value" position="right" className="fill-muted-foreground text-xs" formatter={(value: number) => fmtNum(value)} />
        </Bar>
      </BarChart>
    </ChartContainer>
  );
}

export interface SessionCurveDatum {
  label: string;
  session_count: number;
  avg_cost_per_message_cents: number;
}

export function SessionCurveChart({
  data,
  emptyLabel,
}: {
  data: SessionCurveDatum[];
  emptyLabel: string;
}) {
  if (data.length === 0) {
    return <EmptyState label={emptyLabel} />;
  }

  return (
    <ChartContainer
      config={{
        session_count: {
          label: "Sessions",
          color: "hsl(var(--chart-2))",
        },
        avg_cost_per_message_cents: {
          label: "Avg cost/message",
          color: "hsl(var(--chart-1))",
        },
      }}
    >
      <ComposedChart data={data} margin={{ left: 12, right: 12, top: 6, bottom: 6 }} accessibilityLayer>
        <CartesianGrid vertical={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
        <XAxis dataKey="label" tickLine={false} axisLine={false} />
        <YAxis yAxisId="left" allowDecimals={false} tickFormatter={(value) => fmtNum(value)} tickLine={false} axisLine={false} />
        <YAxis
          yAxisId="right"
          orientation="right"
          tickFormatter={(value) => fmtCost(Number(value) / 100)}
          tickLine={false}
          axisLine={false}
        />
        <ChartTooltip
          cursor={false}
          content={
            <ChartTooltipContent
              indicator="line"
              formatter={(value, name, item) => (
                <div className="flex items-center justify-between gap-2">
                  <span className="text-muted-foreground">{name}</span>
                  <span className="font-medium tabular-nums text-foreground">
                    {item.dataKey === "avg_cost_per_message_cents" ? fmtCost(Number(value) / 100) : fmtNum(Number(value))}
                  </span>
                </div>
              )}
            />
          }
        />
        <ChartLegend content={<ChartLegendContent />} />
        <Bar yAxisId="left" dataKey="session_count" fill="var(--color-session_count)" maxBarSize={36} radius={[4, 4, 0, 0]} />
        <Line yAxisId="right" type="monotone" dataKey="avg_cost_per_message_cents" stroke="var(--color-avg_cost_per_message_cents)" strokeWidth={2} dot={false} />
      </ComposedChart>
    </ChartContainer>
  );
}
