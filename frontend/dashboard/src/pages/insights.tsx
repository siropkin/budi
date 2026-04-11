import { useQuery } from "@tanstack/react-query";
import { AnalyticsFilterBar } from "@/components/analytics-filter-bar";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { CostBarChart, CountBarChart, SessionCurveChart } from "@/components/charts";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchInsights } from "@/lib/api";
import { fmtCost, fmtDurationMs, fmtNum } from "@/lib/format";
import { useDashboardFilters } from "@/lib/period";

const CONFIDENCE_LABELS: Record<string, string> = {
  otel_exact: "OTEL Exact",
  exact: "Exact",
  exact_cost: "Exact Cost",
  estimated: "Estimated",
  estimated_unknown_model: "Estimated (Unknown Model)",
  "n/a": "Not Available",
};

function confidenceLabel(raw: string): string {
  if (raw in CONFIDENCE_LABELS) return CONFIDENCE_LABELS[raw];
  return raw
    .replace(/_/g, " ")
    .replace(/\b\w/g, (c) => c.toUpperCase());
}


function ChartCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle>{title}</CardTitle>
      </CardHeader>
      <CardContent>{children}</CardContent>
    </Card>
  );
}

export function InsightsPage() {
  const { filters } = useDashboardFilters();
  const insightsQuery = useQuery({
    queryKey: ["insights", filters],
    queryFn: ({ signal }) => fetchInsights(filters, signal),
  });

  if (insightsQuery.isPending) {
    return <LoadingState />;
  }

  if (insightsQuery.error) {
    return <ErrorState error={insightsQuery.error} onRetry={() => insightsQuery.refetch()} />;
  }

  const data = insightsQuery.data;

  const confidenceRows = data.confidence
    .filter((row) => row.cost_cents > 0)
    .map((row) => ({
      label: confidenceLabel(row.confidence),
      cost_cents: row.cost_cents,
    }));

  const sessionCurveRows = data.sessionCurve.map((row) => ({
    label: `${row.bucket} msgs`,
    avg_cost_per_message_cents: row.avg_cost_per_message_cents,
    session_count: row.session_count,
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
    sublabel: row.avg_duration_ms != null ? `Avg: ${fmtDurationMs(row.avg_duration_ms)}` : undefined,
  }));


  return (
    <div className="space-y-5">
      <AnalyticsFilterBar />
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

      <ChartCard title="Cost Confidence">
        <CostBarChart data={confidenceRows} emptyLabel="No confidence data for this period" />
      </ChartCard>

      <ChartCard title="Session Length vs Cost">
        <SessionCurveChart data={sessionCurveRows} emptyLabel="No session-cost data for this period" />
      </ChartCard>

      <div className="grid gap-4 md:grid-cols-2">
        <ChartCard title="Speed Mode">
          <CostBarChart data={speedRows} emptyLabel="No speed tags for this period" />
        </ChartCard>

        <ChartCard title="Subagent vs Main">
          <CostBarChart data={subagentRows} emptyLabel="No subagent data for this period" />
        </ChartCard>
      </div>

      <ChartCard title="Tools">
        <CountBarChart data={toolsRows} emptyLabel="No tool usage for this period" valueLabel="calls" />
      </ChartCard>
    </div>
  );
}
