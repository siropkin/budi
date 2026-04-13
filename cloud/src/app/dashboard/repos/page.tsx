import { Suspense } from "react";
import {
  getCurrentUser,
  getCostByRepo,
  getCostByBranch,
  getCostByTicket,
} from "@/lib/dal";
import { dateRangeFromDays } from "@/lib/date-range";
import { repoName } from "@/lib/format";
import { PeriodSelector } from "@/components/period-selector";
import { CostBarChart } from "@/components/charts/cost-bar-chart";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default async function ReposPage({
  searchParams,
}: {
  searchParams: Promise<{ days?: string }>;
}) {
  const params = await searchParams;
  const user = await getCurrentUser();
  if (!user?.org_id) return null;

  const range = dateRangeFromDays(params.days);
  const [repos, branches, tickets] = await Promise.all([
    getCostByRepo(user.org_id, range),
    getCostByBranch(user.org_id, range),
    getCostByTicket(user.org_id, range),
  ]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-bold">Repos</h1>
        <Suspense>
          <PeriodSelector />
        </Suspense>
      </div>

      <div className="grid gap-6 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Cost by Project</CardTitle>
          </CardHeader>
          <CardContent>
            <CostBarChart
              data={repos.map((r) => ({
                label: repoName(r.repo_id),
                cost_cents: r.cost_cents,
              }))}
              emptyLabel="No project data for this period"
            />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Cost by Branch</CardTitle>
          </CardHeader>
          <CardContent>
            <CostBarChart
              data={branches.map((b) => ({
                label: `${repoName(b.repo_id)} / ${b.git_branch.replace(/^refs\/heads\//, "")}`,
                cost_cents: b.cost_cents,
              }))}
              emptyLabel="No branch data for this period"
            />
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Cost by Ticket</CardTitle>
        </CardHeader>
        <CardContent>
          <CostBarChart
            data={tickets.map((t) => ({
              label: t.ticket,
              cost_cents: t.cost_cents,
            }))}
            emptyLabel="No ticket data for this period"
          />
        </CardContent>
      </Card>
    </div>
  );
}
