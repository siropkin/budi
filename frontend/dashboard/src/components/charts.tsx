import {
  Bar,
  BarChart,
  CartesianGrid,
  LabelList,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { ChartContainer, ChartTooltip } from "@/components/ui/chart";
import { EmptyState } from "@/components/state";
import { fmtCost, fmtNum } from "@/lib/format";

const BAR_SIZE = 28;

export interface CostBarDatum {
  label: string;
  cost_cents: number;
}

export function CostBarChart({
  title,
  data,
  emptyLabel,
}: {
  title: string;
  data: CostBarDatum[];
  emptyLabel: string;
}) {
  const sortedData = [...data].sort((left, right) => {
    if (right.cost_cents !== left.cost_cents) return right.cost_cents - left.cost_cents;
    return left.label.localeCompare(right.label);
  });

  if (data.length === 0) {
    return <EmptyState label={emptyLabel} />;
  }

  return (
    <div className="space-y-3">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      <ChartContainer
        config={{
          cost: {
            label: "Cost",
            color: "hsl(var(--chart-1))",
          },
        }}
      >
        <BarChart data={sortedData} layout="vertical" margin={{ left: 20, right: 20, top: 6, bottom: 6 }}>
          <CartesianGrid horizontal={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
          <YAxis dataKey="label" type="category" tickLine={false} axisLine={false} width={140} />
          <XAxis dataKey="cost_cents" type="number" tickFormatter={(value) => fmtCost(value / 100)} axisLine={false} tickLine={false} />
          <ChartTooltip
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null;
              const item = payload[0].payload as CostBarDatum;
              return (
                <div className="rounded-md border border-border bg-card px-3 py-2 text-xs shadow-md">
                  <p className="font-medium text-foreground">{item.label}</p>
                  <p className="text-muted-foreground">{fmtCost((item.cost_cents ?? 0) / 100)}</p>
                </div>
              );
            }}
            cursor={{ fill: "rgba(255,255,255,0.05)" }}
          />
          <Bar dataKey="cost_cents" fill="hsl(var(--chart-1))" barSize={BAR_SIZE} radius={[5, 5, 5, 5]}>
            <LabelList
              dataKey="cost_cents"
              position="right"
              className="fill-muted-foreground text-xs"
              formatter={(value: number) => fmtCost(value / 100)}
            />
          </Bar>
        </BarChart>
      </ChartContainer>
    </div>
  );
}

export interface CountBarDatum {
  label: string;
  value: number;
}

export function CountBarChart({
  title,
  data,
  emptyLabel,
  valueLabel,
}: {
  title: string;
  data: CountBarDatum[];
  emptyLabel: string;
  valueLabel?: string;
}) {
  const sortedData = [...data].sort((left, right) => {
    if (right.value !== left.value) return right.value - left.value;
    return left.label.localeCompare(right.label);
  });

  if (data.length === 0) {
    return <EmptyState label={emptyLabel} />;
  }

  return (
    <div className="space-y-3">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      <ChartContainer
        config={{
          value: {
            label: valueLabel ?? "Value",
            color: "hsl(var(--chart-2))",
          },
        }}
      >
        <BarChart data={sortedData} layout="vertical" margin={{ left: 20, right: 20, top: 6, bottom: 6 }}>
          <CartesianGrid horizontal={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
          <YAxis dataKey="label" type="category" tickLine={false} axisLine={false} width={140} />
          <XAxis dataKey="value" type="number" axisLine={false} tickLine={false} tickFormatter={(value) => fmtNum(value)} />
          <Tooltip
            cursor={{ fill: "rgba(255,255,255,0.05)" }}
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null;
              const item = payload[0].payload as CountBarDatum;
              return (
                <div className="rounded-md border border-border bg-card px-3 py-2 text-xs shadow-md">
                  <p className="font-medium text-foreground">{item.label}</p>
                  <p className="text-muted-foreground">
                    {fmtNum(item.value)} {valueLabel ?? ""}
                  </p>
                </div>
              );
            }}
          />
          <Bar dataKey="value" fill="hsl(var(--chart-2))" barSize={BAR_SIZE} radius={[5, 5, 5, 5]}>
            <LabelList dataKey="value" position="right" className="fill-muted-foreground text-xs" formatter={(value: number) => fmtNum(value)} />
          </Bar>
        </BarChart>
      </ChartContainer>
    </div>
  );
}
