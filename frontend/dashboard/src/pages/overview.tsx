import { useQuery } from "@tanstack/react-query";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ChartContainer, ChartLegend, ChartLegendContent, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { CostBarChart } from "@/components/charts";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchOverview, fetchRegisteredProviders } from "@/lib/api";
import { fmtCost, fmtNum, formatModelName, granularityForPeriod, repoName } from "@/lib/format";
import { usePeriod } from "@/lib/period";

const MAX_BAR_ROWS = 10;

function getActivityTitle(granularity: "hour" | "day" | "month") {
  if (granularity === "hour") return "Activity (Hourly)";
  if (granularity === "month") return "Activity (Monthly)";
  return "Activity (Daily)";
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

export function OverviewPage() {
  const { period } = usePeriod();

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const overviewQuery = useQuery({
    queryKey: ["overview", period.preset],
    queryFn: ({ signal }) => fetchOverview(period, signal),
    refetchInterval: period.preset === "today" ? 30_000 : false,
  });

  if (providersQuery.isPending || overviewQuery.isPending) {
    return <LoadingState />;
  }

  if (providersQuery.error) {
    return <ErrorState error={providersQuery.error} onRetry={() => providersQuery.refetch()} />;
  }

  if (overviewQuery.error) {
    return <ErrorState error={overviewQuery.error} onRetry={() => overviewQuery.refetch()} />;
  }

  const providers = providersQuery.data;
  const data = overviewQuery.data;
  const summary = data.summary;
  const cost = data.cost;

  const totalTokens =
    summary.total_input_tokens +
    summary.total_output_tokens +
    summary.total_cache_creation_tokens +
    summary.total_cache_read_tokens;

  const providerCostRows = providers
    .map((provider) => {
      const stats = data.providers.find((entry) => entry.provider === provider.name);
      const costCents = stats
        ? stats.total_cost_cents != null
          ? stats.total_cost_cents
          : Math.round((stats.estimated_cost ?? 0) * 100)
        : 0;
      return {
        label: provider.display_name,
        cost_cents: costCents,
      };
    })
    .filter((entry) => entry.cost_cents > 0)
    .slice(0, MAX_BAR_ROWS);

  const modelMap = new Map<string, { label: string; cost_cents: number }>();
  for (const model of data.models) {
    const normalizedModel = formatModelName(model.model);
    const providerDisplay = providers.find((entry) => entry.name === model.provider)?.display_name ?? model.provider;
    const key = `${model.provider}:${normalizedModel}`;
    const existing = modelMap.get(key);
    if (existing) {
      existing.cost_cents += model.cost_cents;
    } else {
      modelMap.set(key, {
        label: `${providerDisplay} / ${normalizedModel}`,
        cost_cents: model.cost_cents,
      });
    }
  }
  const modelCostRows = Array.from(modelMap.values())
    .sort((a, b) => b.cost_cents - a.cost_cents)
    .slice(0, MAX_BAR_ROWS);

  const projectCostRows = data.projects
    .map((row) => ({
      label: repoName(row.repo_id),
      cost_cents: row.cost_cents,
    }))
    .slice(0, MAX_BAR_ROWS);

  const branchCostRows = data.branches
    .map((row) => {
      const branch = row.git_branch?.replace(/^refs\/heads\//, "") || "(untagged)";
      return {
        label: `${repoName(row.repo_id)} / ${branch}`,
        cost_cents: row.cost_cents,
      };
    })
    .slice(0, MAX_BAR_ROWS);

  const ticketCostRows = data.tickets.slice(0, MAX_BAR_ROWS).map((row) => ({ label: row.value, cost_cents: row.cost_cents }));
  const activityTypeRows = data.activities.slice(0, MAX_BAR_ROWS).map((row) => ({ label: row.value, cost_cents: row.cost_cents }));

  const activityRows = data.activity.map((entry) => ({
    label: entry.label,
    input_tokens: entry.input_tokens,
    output_tokens: entry.output_tokens,
  }));

  const granularity = granularityForPeriod(period);

  return (
    <div className="space-y-5">
      <div className="grid gap-4 md:grid-cols-3">
        <Card>
          <CardHeader>
            <CardTitle>Total Cost</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold text-primary">{fmtCost(cost.total_cost)}</p>
            <p className="mt-1 text-sm text-muted-foreground">
              {fmtCost(cost.input_cost + cost.cache_write_cost + cost.cache_read_cost)} input+cache / {fmtCost(cost.output_cost)} output
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Tokens</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold">{fmtNum(totalTokens)}</p>
            <p className="mt-1 text-sm text-muted-foreground">
              {fmtNum(summary.total_input_tokens)} input / {fmtNum(summary.total_output_tokens)} output
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Messages</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-3xl font-semibold">{fmtNum(summary.total_messages)}</p>
            <p className="mt-1 text-sm text-muted-foreground">
              {fmtNum(summary.total_user_messages)} input / {fmtNum(summary.total_assistant_messages)} output
            </p>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>{getActivityTitle(granularity)}</CardTitle>
        </CardHeader>
        <CardContent>
          <ChartContainer
            config={{
              input_tokens: { label: "Input", color: "hsl(var(--chart-1))" },
              output_tokens: { label: "Output", color: "hsl(var(--chart-2))" },
            }}
          >
            <BarChart data={activityRows} margin={{ left: 12, right: 8 }} accessibilityLayer>
              <CartesianGrid strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
              <XAxis
                dataKey="label"
                tickFormatter={(value) => {
                  if (granularity === "hour") return value;
                  if (value.length > 8) return value.slice(5);
                  return value;
                }}
                tickLine={false}
                axisLine={false}
              />
              <YAxis tickFormatter={(value) => fmtNum(value)} tickLine={false} axisLine={false} />
              <ChartTooltip
                cursor={false}
                content={
                  <ChartTooltipContent
                    indicator="dot"
                    formatter={(value, name) => (
                      <div className="flex items-center justify-between gap-2">
                        <span className="text-muted-foreground">{name}</span>
                        <span className="font-medium tabular-nums text-foreground">{fmtNum(Number(value))}</span>
                      </div>
                    )}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              <Bar dataKey="input_tokens" fill="var(--color-input_tokens)" maxBarSize={28} radius={[4, 4, 0, 0]} />
              <Bar dataKey="output_tokens" fill="var(--color-output_tokens)" maxBarSize={28} radius={[4, 4, 0, 0]} />
            </BarChart>
          </ChartContainer>
        </CardContent>
      </Card>

      <div className="grid gap-4 md:grid-cols-2">
        <ChartCard title="Agents">
          <CostBarChart data={providerCostRows} emptyLabel="No provider data for this period" />
        </ChartCard>

        <ChartCard title="Models">
          <CostBarChart data={modelCostRows} emptyLabel="No model data for this period" />
        </ChartCard>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <ChartCard title="Projects">
          <CostBarChart data={projectCostRows} emptyLabel="No project data for this period" />
        </ChartCard>

        <ChartCard title="Branches">
          <CostBarChart data={branchCostRows} emptyLabel="No branch data for this period" />
        </ChartCard>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <ChartCard title="Tickets">
          <CostBarChart data={ticketCostRows} emptyLabel="No ticket data for this period" />
        </ChartCard>

        <ChartCard title="Activity Types">
          <CostBarChart data={activityTypeRows} emptyLabel="No activity data for this period" />
        </ChartCard>
      </div>
    </div>
  );
}
