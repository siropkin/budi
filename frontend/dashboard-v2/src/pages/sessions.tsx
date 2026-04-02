import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchRegisteredProviders, fetchSessions } from "@/lib/api";
import { fmtCost, fmtDate, fmtDurationMs, fmtNum, formatModelName, repoName } from "@/lib/format";
import { usePeriod } from "@/lib/period";

const LIMIT = 50;

type SortColumn = "started_at" | "title" | "duration" | "provider" | "model" | "repo_id" | "git_branch" | "tokens" | "cost";

export function SessionsPage() {
  const { period } = usePeriod();
  const navigate = useNavigate();

  const [sortBy, setSortBy] = useState<SortColumn>("started_at");
  const [sortAsc, setSortAsc] = useState(false);
  const [search, setSearch] = useState("");
  const [offset, setOffset] = useState(0);

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const sessionsQuery = useQuery({
    queryKey: ["sessions", period, sortBy, sortAsc, search, offset],
    queryFn: ({ signal }) =>
      fetchSessions(
        period,
        {
          limit: LIMIT,
          offset,
          sort_by: sortBy,
          sort_asc: sortAsc,
          search: search.trim() || undefined,
        },
        signal,
      ),
    placeholderData: (previousData) => previousData,
  });

  const hasMore = useMemo(() => {
    if (!sessionsQuery.data) return false;
    return offset + sessionsQuery.data.sessions.length < sessionsQuery.data.total_count;
  }, [offset, sessionsQuery.data]);

  if (providersQuery.isPending || sessionsQuery.isPending) {
    return <LoadingState />;
  }

  if (providersQuery.error) {
    return <ErrorState error={providersQuery.error} onRetry={() => providersQuery.refetch()} />;
  }

  if (sessionsQuery.error) {
    return <ErrorState error={sessionsQuery.error} onRetry={() => sessionsQuery.refetch()} />;
  }

  const providers = providersQuery.data;
  const sessions = sessionsQuery.data.sessions;
  const totalCount = sessionsQuery.data.total_count;
  const multiProvider = providers.length > 1;

  const onSort = (column: SortColumn) => {
    if (column === sortBy) {
      setSortAsc((previous) => !previous);
      return;
    }

    setSortBy(column);
    setSortAsc(false);
  };

  const sortArrow = (column: SortColumn) => {
    if (column !== sortBy) return null;
    return sortAsc ? "▲" : "▼";
  };

  const onSearchChange: React.ChangeEventHandler<HTMLInputElement> = (event) => {
    setSearch(event.target.value);
    setOffset(0);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>Sessions</CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <Input placeholder="Search sessions..." value={search} onChange={onSearchChange} />

        <div className="overflow-x-auto rounded-md border border-border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="cursor-pointer" onClick={() => onSort("started_at")}>Time {sortArrow("started_at")}</TableHead>
                <TableHead className="cursor-pointer" onClick={() => onSort("title")}>Title {sortArrow("title")}</TableHead>
                <TableHead className="cursor-pointer" onClick={() => onSort("duration")}>Duration {sortArrow("duration")}</TableHead>
                {multiProvider ? (
                  <TableHead className="cursor-pointer" onClick={() => onSort("provider")}>Agent {sortArrow("provider")}</TableHead>
                ) : null}
                <TableHead className="cursor-pointer" onClick={() => onSort("model")}>Model {sortArrow("model")}</TableHead>
                <TableHead className="cursor-pointer" onClick={() => onSort("repo_id")}>Repo {sortArrow("repo_id")}</TableHead>
                <TableHead className="cursor-pointer" onClick={() => onSort("git_branch")}>Branch {sortArrow("git_branch")}</TableHead>
                <TableHead className="cursor-pointer text-right" onClick={() => onSort("tokens")}>Tokens {sortArrow("tokens")}</TableHead>
                <TableHead className="cursor-pointer text-right" onClick={() => onSort("cost")}>Cost {sortArrow("cost")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sessions.map((session) => {
                const rawModel = session.model ?? "";
                const modelList = rawModel
                  .split(",")
                  .map((model) => model.trim())
                  .filter(Boolean);
                const modelSummary =
                  modelList.length > 0
                    ? `${formatModelName(modelList[0])}${modelList.length > 1 ? ` +${modelList.length - 1}` : ""}`
                    : "--";

                const providerDisplay = providers.find((entry) => entry.name === session.provider)?.display_name ?? session.provider;
                const duration = fmtDurationMs(session.duration_ms);
                const tokenCount = (session.input_tokens ?? 0) + (session.output_tokens ?? 0);
                const branch = session.git_branch?.replace(/^refs\/heads\//, "") || "--";

                return (
                  <TableRow
                    key={session.session_id}
                    className="cursor-pointer"
                    onClick={() => navigate(`/sessions/${encodeURIComponent(session.session_id)}`)}
                  >
                    <TableCell className="whitespace-nowrap text-muted-foreground">{fmtDate(session.started_at)}</TableCell>
                    <TableCell className="max-w-[260px] truncate" title={session.title ?? ""}>{session.title || "--"}</TableCell>
                    <TableCell className="text-muted-foreground">{duration}</TableCell>
                    {multiProvider ? <TableCell className="text-muted-foreground">{providerDisplay}</TableCell> : null}
                    <TableCell title={rawModel}>{modelSummary}</TableCell>
                    <TableCell className="max-w-[180px] truncate text-muted-foreground" title={session.repo_id ?? ""}>{repoName(session.repo_id)}</TableCell>
                    <TableCell className="max-w-[180px] truncate text-muted-foreground" title={branch}>{branch}</TableCell>
                    <TableCell className="text-right font-mono text-muted-foreground">{fmtNum(tokenCount)}</TableCell>
                    <TableCell className="text-right font-mono">{fmtCost((session.cost_cents ?? 0) / 100)}</TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </div>

        <div className="flex items-center justify-between text-sm text-muted-foreground">
          <p>
            Showing {sessions.length === 0 ? 0 : offset + 1}-{offset + sessions.length} of {fmtNum(totalCount)}
          </p>
          <div className="flex gap-2">
            <button
              type="button"
              className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50"
              disabled={offset === 0}
              onClick={() => setOffset((previous) => Math.max(0, previous - LIMIT))}
            >
              Previous
            </button>
            <button
              type="button"
              className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-muted disabled:cursor-not-allowed disabled:opacity-50"
              disabled={!hasMore}
              onClick={() => setOffset((previous) => previous + LIMIT)}
            >
              Next
            </button>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
