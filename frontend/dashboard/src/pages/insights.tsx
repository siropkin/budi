import { useQuery } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { CostBarChart, CountBarChart } from "@/components/charts";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchInsights } from "@/lib/api";
import { fmtCost, fmtNum } from "@/lib/format";
import { usePeriod } from "@/lib/period";

const CONFIDENCE_LABELS: Record<string, string> = {
  otel_exact: "OTEL Exact",
  exact: "Exact",
  exact_cost: "Exact Cost",
  estimated: "Estimated",
};

function mcpName(raw: string): string {
  const normalized = raw.replace(/^mcp__/, "");
  const parts = normalized.split("__");
  if (parts.length >= 2) {
    return `${parts[0]} / ${parts.slice(1).join("/")}`;
  }
  return raw;
}

export function InsightsPage() {
  const { period } = usePeriod();
  const insightsQuery = useQuery({
    queryKey: ["insights", period.preset, period.from ?? "", period.to ?? ""],
    queryFn: ({ signal }) => fetchInsights(period, signal),
  });

  if (insightsQuery.isPending) {
    return <LoadingState />;
  }

  if (insightsQuery.error) {
    return <ErrorState error={insightsQuery.error} onRetry={() => insightsQuery.refetch()} />;
  }

  const data = insightsQuery.data;

  const confidenceRows = data.confidence.map((row) => ({
    label: CONFIDENCE_LABELS[row.confidence] ?? row.confidence,
    cost_cents: row.cost_cents,
  }));

  const sessionCostRows = data.sessionCurve.map((row) => ({
    label: `${row.bucket} msgs`,
    cost_cents: row.avg_cost_per_message_cents,
  }));

  const sessionCountRows = data.sessionCurve.map((row) => ({
    label: `${row.bucket} msgs`,
    value: row.session_count,
  }));

  const speedRows = data.speedTags
    .filter((row) => row.value !== "(untagged)")
    .map((row) => ({
      label:
        row.value === "fast"
          ? "Fast (6x cost)"
          : row.value === "normal"
            ? "Normal"
            : row.value,
      cost_cents: row.cost_cents,
    }));

  const subagentRows = data.subagent.map((row) => ({
    label: row.category === "main" ? "Main conversation" : "Subagents",
    cost_cents: row.cost_cents,
  }));

  const toolsRows = data.tools.map((row) => ({
    label: row.tool_name,
    value: row.call_count,
  }));

  const mcpRows = data.mcp.map((row) => ({
    label: mcpName(row.tool_name),
    value: row.call_count,
  }));

  return (
    <div className="space-y-5">
      <Card>
        <CardContent className="pt-5">
          <CostBarChart title="Cost Confidence" data={confidenceRows} emptyLabel="No confidence data for this period" />
        </CardContent>
      </Card>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Cache Savings</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold text-primary">{fmtCost((data.cacheEff?.cache_savings_cents ?? 0) / 100)}</p>
            <p className="mt-1 text-sm text-muted-foreground">{((data.cacheEff?.cache_hit_rate ?? 0) * 100).toFixed(1)}% cache hit rate</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Cache Read Tokens</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold">{fmtNum(data.cacheEff?.total_cache_read_tokens ?? 0)}</p>
            <p className="mt-1 text-sm text-muted-foreground">{fmtNum(data.cacheEff?.total_cache_creation_tokens ?? 0)} cache writes</p>
          </CardContent>
        </Card>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CostBarChart
              title="Avg Cost per Message by Session Length"
              data={sessionCostRows}
              emptyLabel="No session-cost data for this period"
            />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CountBarChart
              title="Sessions by Length"
              data={sessionCountRows}
              emptyLabel="No session-count data for this period"
              valueLabel="sessions"
            />
          </CardContent>
        </Card>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Speed Mode" data={speedRows} emptyLabel="No speed tags for this period" />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Subagent vs Main" data={subagentRows} emptyLabel="No subagent data for this period" />
          </CardContent>
        </Card>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CountBarChart title="Tools" data={toolsRows} emptyLabel="No tool usage for this period" valueLabel="calls" />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CountBarChart title="MCP Servers" data={mcpRows} emptyLabel="No MCP usage for this period" valueLabel="calls" />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
