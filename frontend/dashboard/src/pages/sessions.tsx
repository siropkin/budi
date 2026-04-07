import { useCallback, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { ArrowDown, ArrowUp, Download } from "lucide-react";
import { toast } from "sonner";
import { AnalyticsFilterBar } from "@/components/analytics-filter-bar";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchAllSessions, fetchRegisteredProviders, fetchSessions } from "@/lib/api";
import { buildExportFilename, type CsvColumn, downloadCsv, toCsv } from "@/lib/csv";
import { fmtCost, fmtDate, fmtDurationMs, fmtNum, formatModelName, repoName } from "@/lib/format";
import { useDashboardFilters } from "@/lib/period";
import type { SessionRow } from "@/lib/types";
import { cn } from "@/lib/utils";

const LIMIT = 50;

type SortColumn = "started_at" | "title" | "duration" | "provider" | "model" | "repo_id" | "git_branch" | "tokens" | "cost";
const SESSION_CELL_CLASS = "align-top text-sm text-foreground whitespace-normal break-words";

function SortableHead({
  label,
  column,
  sortBy,
  sortAsc,
  onSort,
  right = false,
}: {
  label: string;
  column: SortColumn;
  sortBy: SortColumn;
  sortAsc: boolean;
  onSort: (column: SortColumn) => void;
  right?: boolean;
}) {
  const isActive = sortBy === column;
  return (
    <TableHead aria-sort={isActive ? (sortAsc ? "ascending" : "descending") : "none"} className={right ? "text-right" : undefined}>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className={cn(
          "h-8 px-1 text-muted-foreground hover:bg-transparent hover:text-foreground focus-visible:ring-ring",
          isActive && "text-primary",
          right && "ml-auto",
        )}
        onClick={() => onSort(column)}
      >
        {label}
        {isActive ? (
          sortAsc ? <ArrowUp className="ml-1 h-3.5 w-3.5" aria-hidden="true" /> : <ArrowDown className="ml-1 h-3.5 w-3.5" aria-hidden="true" />
        ) : null}
      </Button>
    </TableHead>
  );
}

export function SessionsPage() {
  const { filters } = useDashboardFilters();
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
    queryKey: ["sessions", filters, sortBy, sortAsc, search, offset],
    queryFn: ({ signal }) =>
      fetchSessions(
        filters,
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

  const exportingRef = useRef(false);

  const SESSION_CSV_COLUMNS: CsvColumn<SessionRow>[] = useMemo(
    () => [
      { header: "Time", value: (s) => s.started_at },
      { header: "Title", value: (s) => s.title },
      { header: "Duration (ms)", value: (s) => s.duration_ms },
      { header: "Agent", value: (s) => s.provider },
      { header: "Model", value: (s) => (s.models ?? []).join(", ") },
      { header: "Repo", value: (s) => (s.repo_ids ?? [])[0] },
      { header: "Branch", value: (s) => (s.git_branches ?? [])[0]?.replace(/^refs\/heads\//, "") },
      { header: "Input Tokens", value: (s) => s.input_tokens },
      { header: "Output Tokens", value: (s) => s.output_tokens },
      { header: "Cost ($)", value: (s) => ((s.cost_cents ?? 0) / 100).toFixed(4) },
    ],
    [],
  );

  const handleExportCsv = useCallback(async () => {
    if (exportingRef.current) return;
    exportingRef.current = true;
    const toastId = toast.loading("Exporting sessions...");
    try {
      const allSessions = await fetchAllSessions(filters, search.trim() || undefined);
      const csv = toCsv(allSessions, SESSION_CSV_COLUMNS);
      downloadCsv(csv, buildExportFilename("sessions", filters));
      toast.success(`Exported ${allSessions.length} sessions`, { id: toastId });
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Export failed", { id: toastId });
    } finally {
      exportingRef.current = false;
    }
  }, [filters, search, SESSION_CSV_COLUMNS]);

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

  return (
    <div className="space-y-5">
      <AnalyticsFilterBar />
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle>Sessions</CardTitle>
          <Button type="button" variant="outline" size="sm" onClick={handleExportCsv} disabled={sessions.length === 0}>
            <Download className="mr-1.5 h-3.5 w-3.5" aria-hidden="true" />
            Export CSV
          </Button>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="overflow-hidden rounded-md border border-border bg-background p-1">
            <Table className="table-fixed">
              <TableHeader>
                <TableRow>
                  <SortableHead label="Time" column="started_at" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Title" column="title" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Duration" column="duration" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  {multiProvider ? (
                    <SortableHead label="Agent" column="provider" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  ) : null}
                  <SortableHead label="Model" column="model" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Repo" column="repo_id" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Branch" column="git_branch" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Tokens" column="tokens" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                  <SortableHead label="Cost" column="cost" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                </TableRow>
              </TableHeader>
              <TableBody>
                {sessions.map((session) => {
                  const modelList = session.models ?? [];
                  const rawModel = modelList.join(", ");
                  const modelSummary =
                    modelList.length > 0
                      ? `${formatModelName(modelList[0])}${modelList.length > 1 ? ` +${modelList.length - 1}` : ""}`
                      : "--";

                  const providerDisplay = providers.find((entry) => entry.name === session.provider)?.display_name ?? session.provider;
                  const duration = fmtDurationMs(session.duration_ms);
                  const tokenCount = (session.input_tokens ?? 0) + (session.output_tokens ?? 0);
                  const repoIds = session.repo_ids ?? [];
                  const gitBranches = session.git_branches ?? [];
                  const primaryRepo = repoIds[0] ?? null;
                  const primaryBranch = gitBranches[0] ?? null;
                  const branch = primaryBranch?.replace(/^refs\/heads\//, "") || "--";
                  const repoLabel =
                    repoIds.length > 1 ? `${repoName(primaryRepo)} +${repoIds.length - 1}` : repoName(primaryRepo);
                  const branchLabel = gitBranches.length > 1 ? `${branch} +${gitBranches.length - 1}` : branch;

                  return (
                    <TableRow
                      key={session.id}
                      role="link"
                      tabIndex={0}
                      className="cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                      onClick={() => navigate(`/sessions/${encodeURIComponent(session.id)}`)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          navigate(`/sessions/${encodeURIComponent(session.id)}`);
                        }
                      }}
                    >
                      <TableCell className={SESSION_CELL_CLASS}>{fmtDate(session.started_at)}</TableCell>
                      <TableCell className={SESSION_CELL_CLASS} title={session.title ?? ""}>{session.title || "--"}</TableCell>
                      <TableCell className={SESSION_CELL_CLASS}>{duration}</TableCell>
                      {multiProvider ? <TableCell className={SESSION_CELL_CLASS}>{providerDisplay}</TableCell> : null}
                      <TableCell className={SESSION_CELL_CLASS} title={rawModel}>{modelSummary}</TableCell>
                      <TableCell className={SESSION_CELL_CLASS} title={primaryRepo ?? ""}>{repoLabel}</TableCell>
                      <TableCell className={SESSION_CELL_CLASS} title={branch}>{branchLabel}</TableCell>
                      <TableCell className={`${SESSION_CELL_CLASS} whitespace-nowrap text-right`}>{fmtNum(tokenCount)}</TableCell>
                      <TableCell className={`${SESSION_CELL_CLASS} whitespace-nowrap text-right`}>{fmtCost((session.cost_cents ?? 0) / 100)}</TableCell>
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
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={offset === 0}
                onClick={() => setOffset((previous) => Math.max(0, previous - LIMIT))}
              >
                Previous
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={!hasMore}
                onClick={() => setOffset((previous) => previous + LIMIT)}
              >
                Next
              </Button>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
