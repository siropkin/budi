import { Suspense } from "react";
import { getCurrentUser, getCostByUser } from "@/lib/dal";
import { dateRangeFromDays } from "@/lib/date-range";
import { fmtCost } from "@/lib/format";
import { PeriodSelector } from "@/components/period-selector";
import { CostBarChart } from "@/components/charts/cost-bar-chart";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default async function TeamPage({
  searchParams,
}: {
  searchParams: Promise<{ days?: string }>;
}) {
  const params = await searchParams;
  const user = await getCurrentUser();
  if (!user?.org_id) return null;

  const range = dateRangeFromDays(params.days);
  const userCosts = await getCostByUser(user.org_id, range);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-bold">Team</h1>
        <Suspense>
          <PeriodSelector />
        </Suspense>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Cost by Team Member</CardTitle>
        </CardHeader>
        <CardContent>
          <CostBarChart
            data={userCosts.map((u) => ({
              label: u.name,
              cost_cents: u.cost_cents,
            }))}
            emptyLabel="No team cost data for this period"
          />
        </CardContent>
      </Card>

      {userCosts.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>Team Members</CardTitle>
          </CardHeader>
          <CardContent>
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-white/10 text-left text-zinc-400">
                  <th className="pb-2 font-medium">Name</th>
                  <th className="pb-2 text-right font-medium">Cost</th>
                </tr>
              </thead>
              <tbody>
                {userCosts.map((u, i) => (
                  <tr key={i} className="border-b border-white/5">
                    <td className="py-2 text-zinc-200">{u.name}</td>
                    <td className="py-2 text-right tabular-nums text-zinc-300">
                      {fmtCost(u.cost_cents)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
