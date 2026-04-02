import { useQuery } from "@tanstack/react-query";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ChartContainer, ChartTooltip } from "@/components/ui/chart";
import { CostBarChart } from "@/components/charts";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchOverview, fetchRegisteredProviders } from "@/lib/api";
import { fmtCost, fmtNum, formatModelName, granularityForPeriod, repoName } from "@/lib/format";
import { usePeriod } from "@/lib/period";

function getActivityTitle(granularity: "hour" | "day" | "month") {
  if (granularity === "hour") return "Activity (Hourly)";
  if (granularity === "month") return "Activity (Monthly)";
  return "Activity (Daily)";
}

export function OverviewPage() {
  const { period } = usePeriod();

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const overviewQuery = useQuery({
    queryKey: ["overview", period.preset, period.from ?? "", period.to ?? ""],
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
    .slice(0, 15);

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
    .slice(0, 15);

  const projectCostRows = data.projects
    .map((row) => ({
      label: repoName(row.repo_id),
      cost_cents: row.cost_cents,
    }))
    .slice(0, 15);

  const branchCostRows = data.branches
    .map((row) => {
      const branch = row.git_branch?.replace(/^refs\/heads\//, "") || "(untagged)";
      return {
        label: `${repoName(row.repo_id)} / ${branch}`,
        cost_cents: row.cost_cents,
      };
    })
    .slice(0, 15);

  const ticketCostRows = data.tickets.slice(0, 15).map((row) => ({ label: row.value, cost_cents: row.cost_cents }));
  const activityTypeRows = data.activities.slice(0, 15).map((row) => ({ label: row.value, cost_cents: row.cost_cents }));

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
              input: { label: "Input", color: "hsl(var(--chart-1))" },
              output: { label: "Output", color: "hsl(var(--chart-2))" },
            }}
          >
            <BarChart data={activityRows} margin={{ left: 12, right: 8 }}>
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
                cursor={{ fill: "rgba(255,255,255,0.05)" }}
                content={({ active, payload, label }) => {
                  if (!active || !payload || payload.length === 0) return null;
                  const input = Number(payload.find((item) => item.dataKey === "input_tokens")?.value ?? 0);
                  const output = Number(payload.find((item) => item.dataKey === "output_tokens")?.value ?? 0);
                  return (
                    <div className="rounded-md border border-border bg-card px-3 py-2 text-xs shadow-md">
                      <p className="font-medium">{label}</p>
                      <p className="text-muted-foreground">Input: {fmtNum(input)}</p>
                      <p className="text-muted-foreground">Output: {fmtNum(output)}</p>
                    </div>
                  );
                }}
              />
              <Bar dataKey="input_tokens" fill="var(--color-input)" maxBarSize={28} radius={[4, 4, 0, 0]} />
              <Bar dataKey="output_tokens" fill="var(--color-output)" maxBarSize={28} radius={[4, 4, 0, 0]} />
            </BarChart>
          </ChartContainer>
        </CardContent>
      </Card>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Agents" data={providerCostRows} emptyLabel="No provider data for this period" />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Models" data={modelCostRows} emptyLabel="No model data for this period" />
          </CardContent>
        </Card>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Projects" data={projectCostRows} emptyLabel="No project data for this period" />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Branches" data={branchCostRows} emptyLabel="No branch data for this period" />
          </CardContent>
        </Card>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Tickets" data={ticketCostRows} emptyLabel="No ticket data for this period" />
          </CardContent>
        </Card>

        <Card>
          <CardContent className="pt-5">
            <CostBarChart title="Activity Types" data={activityTypeRows} emptyLabel="No activity data for this period" />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
