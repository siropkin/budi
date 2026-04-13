import { Suspense } from "react";
import { getCurrentUser, getOverviewStats, getDailyActivity } from "@/lib/dal";
import { dateRangeFromDays } from "@/lib/date-range";
import { fmtCost, fmtNum } from "@/lib/format";
import { StatCard } from "@/components/stat-card";
import { PeriodSelector } from "@/components/period-selector";
import { ActivityChart } from "@/components/charts/activity-chart";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default async function OverviewPage({
  searchParams,
}: {
  searchParams: Promise<{ days?: string }>;
}) {
  const params = await searchParams;
  const user = await getCurrentUser();
  if (!user?.org_id) return null;

  const range = dateRangeFromDays(params.days);
  const [stats, activity] = await Promise.all([
    getOverviewStats(user.org_id, range),
    getDailyActivity(user.org_id, range),
  ]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-bold">Overview</h1>
        <Suspense>
          <PeriodSelector />
        </Suspense>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          title="Total Cost"
          value={fmtCost(stats.totalCostCents)}
        />
        <StatCard
          title="Total Tokens"
          value={fmtNum(stats.totalInputTokens + stats.totalOutputTokens)}
          subtitle={`${fmtNum(stats.totalInputTokens)} in / ${fmtNum(stats.totalOutputTokens)} out`}
        />
        <StatCard
          title="Messages"
          value={fmtNum(stats.totalMessages)}
        />
        <StatCard
          title="Sessions"
          value={fmtNum(stats.totalSessions)}
        />
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Daily Activity (Tokens)</CardTitle>
        </CardHeader>
        <CardContent>
          <ActivityChart data={activity} />
        </CardContent>
      </Card>
    </div>
  );
}
