"use client";

import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { fmtDate, fmtNum } from "@/lib/format";

interface ActivityData {
  bucket_day: string;
  input_tokens: number;
  output_tokens: number;
  cost_cents: number;
  message_count: number;
}

export function ActivityChart({ data }: { data: ActivityData[] }) {
  if (data.length === 0) {
    return (
      <div className="flex h-64 items-center justify-center text-sm text-zinc-500">
        No activity data for this period
      </div>
    );
  }

  return (
    <ResponsiveContainer width="100%" height={300}>
      <BarChart data={data} margin={{ left: 12, right: 8, top: 8, bottom: 8 }}>
        <CartesianGrid
          strokeDasharray="3 3"
          stroke="rgba(255,255,255,0.06)"
          vertical={false}
        />
        <XAxis
          dataKey="bucket_day"
          tickFormatter={fmtDate}
          tick={{ fill: "#71717a", fontSize: 12 }}
          tickLine={false}
          axisLine={false}
        />
        <YAxis
          tickFormatter={fmtNum}
          tick={{ fill: "#71717a", fontSize: 12 }}
          tickLine={false}
          axisLine={false}
        />
        <Tooltip
          contentStyle={{
            background: "#18181b",
            border: "1px solid rgba(255,255,255,0.1)",
            borderRadius: "8px",
            fontSize: "13px",
          }}
          labelFormatter={(label) => fmtDate(String(label))}
          formatter={(value, name) => [
            fmtNum(Number(value)),
            String(name) === "input_tokens" ? "Input" : "Output",
          ]}
        />
        <Bar
          dataKey="input_tokens"
          stackId="tokens"
          fill="#3b82f6"
          maxBarSize={28}
          radius={[0, 0, 0, 0]}
        />
        <Bar
          dataKey="output_tokens"
          stackId="tokens"
          fill="#8b5cf6"
          maxBarSize={28}
          radius={[4, 4, 0, 0]}
        />
      </BarChart>
    </ResponsiveContainer>
  );
}
