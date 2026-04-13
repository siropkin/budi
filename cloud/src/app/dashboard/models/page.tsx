import { Suspense } from "react";
import { getCurrentUser, getCostByModel } from "@/lib/dal";
import { dateRangeFromDays } from "@/lib/date-range";
import { formatModelName } from "@/lib/format";
import { PeriodSelector } from "@/components/period-selector";
import { CostBarChart } from "@/components/charts/cost-bar-chart";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default async function ModelsPage({
  searchParams,
}: {
  searchParams: Promise<{ days?: string }>;
}) {
  const params = await searchParams;
  const user = await getCurrentUser();
  if (!user?.org_id) return null;

  const range = dateRangeFromDays(params.days);
  const models = await getCostByModel(user.org_id, range);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-bold">Models</h1>
        <Suspense>
          <PeriodSelector />
        </Suspense>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Cost by Model</CardTitle>
        </CardHeader>
        <CardContent>
          <CostBarChart
            data={models.map((m) => ({
              label: `${m.provider} / ${formatModelName(m.model)}`,
              cost_cents: m.cost_cents,
            }))}
            emptyLabel="No model data for this period"
          />
        </CardContent>
      </Card>
    </div>
  );
}
