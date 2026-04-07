import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router-dom";
import { Bar, CartesianGrid, ComposedChart, Line, XAxis, YAxis } from "recharts";
import { ArrowDown, ArrowLeft, ArrowUp, Download } from "lucide-react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ChartContainer, ChartLegend, ChartLegendContent, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { ErrorState, LoadingState } from "@/components/state";
import {
  fetchAllSessionMessages,
  fetchRegisteredProviders,
  fetchSessionDetail,
  fetchSessionHealth,
  fetchSessionMessageCurve,
  fetchSessionMessagesWithRoles,
  fetchSessionTags,
} from "@/lib/api";
import { type CsvColumn, downloadCsv, toCsv } from "@/lib/csv";
import { fmtCost, fmtDate, fmtNum, formatModelName, repoName } from "@/lib/format";
import type { MessageRow, MessagesResponse } from "@/lib/types";
import { cn } from "@/lib/utils";

function healthVariant(state: string): "default" | "warning" | "success" {
  if (state === "red") return "default";
  if (state === "yellow") return "warning";
  return "success";
}

const MESSAGE_CELL_CLASS = "align-top text-sm text-foreground whitespace-normal break-words";
const MESSAGE_PAGE_SIZE = 50;
type MessageSortColumn = "timestamp" | "provider" | "model" | "repo_id" | "git_branch" | "tokens" | "cost";
const HEALTH_VITAL_ORDER: Array<{ key: string; label: string }> = [
  { key: "context_drag", label: "Context growth" },
  { key: "cache_efficiency", label: "Cache reuse" },
  { key: "cost_acceleration", label: "Cost acceleration" },
  { key: "thrashing", label: "Retry loops" },
];

function compactTools(tools: string[] | undefined, max = 2): string {
  const values = (tools ?? []).filter(Boolean);
  if (values.length === 0) return "--";
  if (values.length <= max) return values.join(", ");
  return `${values.slice(0, max).join(", ")} +${values.length - max}`;
}

function SortableHead({
  label,
  column,
  sortBy,
  sortAsc,
  onSort,
  right = false,
}: {
  label: string;
  column: MessageSortColumn;
  sortBy: MessageSortColumn;
  sortAsc: boolean;
  onSort: (column: MessageSortColumn) => void;
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

export function SessionDetailPage() {
  const params = useParams<{ sessionId: string }>();
  const navigate = useNavigate();
  const sessionId = params.sessionId;
  const [sortBy, setSortBy] = useState<MessageSortColumn>("timestamp");
  const [sortAsc, setSortAsc] = useState(false);
  const [messageOffset, setMessageOffset] = useState(0);

  const detailQuery = useQuery({
    queryKey: ["session-detail", sessionId],
    queryFn: ({ signal }) => fetchSessionDetail(sessionId ?? "", signal),
    enabled: Boolean(sessionId),
  });

  const messagesQuery = useQuery<MessagesResponse>({
    queryKey: ["session-messages", sessionId, sortBy, sortAsc, messageOffset],
    queryFn: ({ signal }) =>
      fetchSessionMessagesWithRoles(
        sessionId ?? "",
        "assistant",
        {
          limit: MESSAGE_PAGE_SIZE,
          offset: messageOffset,
          sort_by: sortBy,
          sort_asc: sortAsc,
        },
        signal,
      ),
    placeholderData: (previousData) => previousData,
    enabled: Boolean(sessionId),
  });

  const tagsQuery = useQuery({
    queryKey: ["session-tags", sessionId],
    queryFn: ({ signal }) => fetchSessionTags(sessionId ?? "", signal),
    enabled: Boolean(sessionId),
  });

  const healthQuery = useQuery({
    queryKey: ["session-health", sessionId],
    queryFn: ({ signal }) => fetchSessionHealth(sessionId ?? "", signal),
    enabled: Boolean(sessionId),
  });

  const messageCurveQuery = useQuery({
    queryKey: ["session-message-curve", sessionId],
    queryFn: ({ signal }) => fetchSessionMessageCurve(sessionId ?? "", signal),
    enabled: Boolean(sessionId),
  });

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const messagePage = messagesQuery.data?.messages ?? [];
  const messageTotalCount = messagesQuery.data?.total_count ?? 0;
  const providers = providersQuery.data ?? [];

  const exportingRef = useRef(false);

  const MESSAGE_CSV_COLUMNS: CsvColumn<MessageRow>[] = useMemo(
    () => [
      { header: "Time", value: (m) => m.timestamp },
      { header: "#", value: (m) => m.assistant_sequence },
      { header: "Agent", value: (m) => m.provider },
      { header: "Model", value: (m) => m.model },
      { header: "Repo", value: (m) => m.repo_id },
      { header: "Branch", value: (m) => m.git_branch?.replace(/^refs\/heads\//, "") },
      { header: "Tools", value: (m) => (m.tools ?? []).join(", ") },
      { header: "Tags", value: (m) => (m.tags ?? []).map((t) => `${t.key}:${t.value}`).join(", ") },
      { header: "Input Tokens", value: (m) => m.input_tokens },
      { header: "Output Tokens", value: (m) => m.output_tokens },
      { header: "Cost ($)", value: (m) => ((m.cost_cents ?? 0) / 100).toFixed(4) },
      { header: "Cost Confidence", value: (m) => m.cost_confidence },
    ],
    [],
  );

  const handleExportMessages = useCallback(async () => {
    if (exportingRef.current || !sessionId) return;
    exportingRef.current = true;
    const toastId = toast.loading("Exporting messages...");
    try {
      const allMessages = await fetchAllSessionMessages(sessionId);
      const csv = toCsv(allMessages, MESSAGE_CSV_COLUMNS);
      const safeTitle = (detailQuery.data?.title ?? "session").replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 40);
      const date = new Date().toISOString().slice(0, 10);
      downloadCsv(csv, `messages_${safeTitle}_${date}.csv`);
      toast.success(`Exported ${allMessages.length} messages`, { id: toastId });
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Export failed", { id: toastId });
    } finally {
      exportingRef.current = false;
    }
  }, [sessionId, detailQuery.data?.title, MESSAGE_CSV_COLUMNS]);

  useEffect(() => {
    setMessageOffset(0);
  }, [sortBy, sortAsc, sessionId]);

  useEffect(() => {
    if (messageOffset >= messageTotalCount && messageOffset > 0) {
      const previousPageOffset = Math.max(0, Math.floor((messageTotalCount - 1) / MESSAGE_PAGE_SIZE) * MESSAGE_PAGE_SIZE);
      setMessageOffset(previousPageOffset);
    }
  }, [messageOffset, messageTotalCount]);

  if (!sessionId) {
    return <ErrorState error={new Error("Session ID is missing in route")} />;
  }

  if (
    messagesQuery.isPending ||
    tagsQuery.isPending ||
    healthQuery.isPending ||
    messageCurveQuery.isPending ||
    providersQuery.isPending ||
    detailQuery.isPending
  ) {
    return <LoadingState label="Loading session detail..." />;
  }

  const detailErrorMessage = detailQuery.error?.message?.toLowerCase() ?? "";
  const isSessionNotFound = detailErrorMessage.includes("session") && detailErrorMessage.includes("not found");
  if (isSessionNotFound) {
    return (
      <div className="space-y-5">
        <Card>
          <CardHeader>
            <CardTitle>Session Not Found</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2 text-sm text-muted-foreground">
            <p>We could not find a session for this ID.</p>
            <p className="font-mono text-foreground">{decodeURIComponent(sessionId)}</p>
            <p>The session may have been deleted, or the ID may be invalid.</p>
          </CardContent>
        </Card>
      </div>
    );
  }

  if (detailQuery.error) {
    return <ErrorState error={detailQuery.error} onRetry={() => detailQuery.refetch()} />;
  }

  if (messagesQuery.error) {
    return <ErrorState error={messagesQuery.error} onRetry={() => messagesQuery.refetch()} />;
  }

  if (tagsQuery.error) {
    return <ErrorState error={tagsQuery.error} onRetry={() => tagsQuery.refetch()} />;
  }

  if (healthQuery.error) {
    return <ErrorState error={healthQuery.error} onRetry={() => healthQuery.refetch()} />;
  }

  if (messageCurveQuery.error) {
    return <ErrorState error={messageCurveQuery.error} onRetry={() => messageCurveQuery.refetch()} />;
  }

  if (providersQuery.error) {
    return <ErrorState error={providersQuery.error} onRetry={() => providersQuery.refetch()} />;
  }

  const sessionDetail = detailQuery.data;
  if (!sessionDetail) {
    return <ErrorState error={new Error("Session detail is unavailable")} />;
  }

  const tags = tagsQuery.data ?? [];
  const health = healthQuery.data;
  const healthVitals = (health?.vitals ?? {}) as Record<string, { state: string; label: string } | undefined>;

  const tokenTotal = (sessionDetail.input_tokens ?? 0) + (sessionDetail.output_tokens ?? 0);
  const costTotalCents = sessionDetail.cost_cents ?? 0;

  const sessionCurve = (messageCurveQuery.data ?? []).map((point) => ({
    label: `#${point.assistant_sequence}`,
    tokens: point.tokens,
    cumulative_cost_cents: point.cumulative_cost_cents,
  }));

  const paginatedMessages = messagePage;
  const hasMoreMessages = messageOffset + paginatedMessages.length < messageTotalCount;
  const overviewRepos = sessionDetail.repo_ids ?? [];
  const overviewRepoPrimary = overviewRepos[0] ?? null;
  const overviewRepoLabel =
    overviewRepos.length > 1
      ? `${repoName(overviewRepoPrimary)} +${overviewRepos.length - 1}`
      : repoName(overviewRepoPrimary);

  const overviewBranches = sessionDetail.git_branches ?? [];
  const rawOverviewBranch = overviewBranches[0] ?? null;
  const overviewBranchPrimary = rawOverviewBranch?.replace(/^refs\/heads\//, "") ?? "--";
  const overviewBranchLabel =
    overviewBranches.length > 1
      ? `${overviewBranchPrimary} +${overviewBranches.length - 1}`
      : overviewBranchPrimary;
  const overviewName = sessionDetail.title || "--";

  const onSort = (column: MessageSortColumn) => {
    if (column === sortBy) {
      setSortAsc((previous) => !previous);
      return;
    }
    setSortBy(column);
    setSortAsc(false);
  };
  const showProviderColumn =
    new Set(paginatedMessages.map((message) => message.provider).filter(Boolean)).size > 1;

  return (
    <div className="space-y-5">
      <div className="rounded-lg border bg-card px-4 py-4 md:px-6">
        <Button type="button" variant="ghost" size="sm" className="mb-3 h-8 px-2 text-muted-foreground" onClick={() => navigate("/sessions")}>
          <ArrowLeft className="mr-1.5 h-4 w-4" aria-hidden="true" />
          Back to Sessions
        </Button>
        <div className="space-y-1">
          <h2 className="text-xl font-semibold tracking-tight text-foreground">{overviewName}</h2>
          <p className="text-xs text-muted-foreground">
            Session ID: <span className="font-mono">{decodeURIComponent(sessionId)}</span>
          </p>
        </div>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Overview</CardTitle>
          </CardHeader>
          <CardContent className="space-y-1 text-sm">
            <p>
              Cost: <span className="font-semibold text-primary">{fmtCost(costTotalCents / 100)}</span>
            </p>
            <p>
              Tokens: <span className="font-semibold text-foreground">{fmtNum(tokenTotal)}</span>
            </p>
            <p>
              Messages: <span className="font-semibold text-foreground">{fmtNum(sessionDetail.message_count)}</span>
            </p>
            <p>
              Repo: <span className="font-semibold text-foreground">{overviewRepoLabel}</span>
            </p>
            <p>
              Branch: <span className="font-semibold text-foreground">{overviewBranchLabel}</span>
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Session Health</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-2">
            {HEALTH_VITAL_ORDER.map((vitalMeta) => {
              const vital = healthVitals[vitalMeta.key];
              return (
                <div key={vitalMeta.key} className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2">
                  <span className="text-sm text-muted-foreground">{vitalMeta.label}</span>
                  {vital ? (
                    <Badge variant={healthVariant(vital.state)}>{vital.label}</Badge>
                  ) : (
                    <Badge variant="outline" className="border-muted text-muted-foreground">
                      No data
                    </Badge>
                  )}
                </div>
              );
            })}
          </CardContent>
        </Card>
      </div>

      {tags.length > 0 ? (
        <Card>
          <CardHeader>
            <CardTitle>Tags</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-wrap gap-2">
            {tags.map((tag) => (
              <Badge key={`${tag.key}:${tag.value}`} variant="outline">
                {tag.key}: {tag.value}
              </Badge>
            ))}
          </CardContent>
        </Card>
      ) : null}

      <Card>
        <CardHeader>
          <CardTitle>Session Length vs Cost</CardTitle>
        </CardHeader>
        <CardContent>
          {sessionCurve.length === 0 ? (
            <p className="text-sm text-muted-foreground">No assistant messages available.</p>
          ) : (
            <ChartContainer
              config={{
                tokens: {
                  label: "Tokens/message",
                  color: "hsl(var(--chart-2))",
                },
                cumulative_cost_cents: {
                  label: "Cumulative cost",
                  color: "hsl(var(--chart-1))",
                },
              }}
            >
              <ComposedChart data={sessionCurve} margin={{ left: 12, right: 12, top: 6, bottom: 6 }} accessibilityLayer>
                <CartesianGrid vertical={false} strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
                <XAxis dataKey="label" tickLine={false} axisLine={false} />
                <YAxis yAxisId="left" tickFormatter={(value) => fmtNum(Number(value))} tickLine={false} axisLine={false} />
                <YAxis
                  yAxisId="right"
                  orientation="right"
                  tickFormatter={(value) => fmtCost(Number(value) / 100)}
                  tickLine={false}
                  axisLine={false}
                />
                <ChartTooltip
                  cursor={false}
                  content={
                    <ChartTooltipContent
                      indicator="line"
                      formatter={(value, name, item) => (
                        <div className="flex items-center justify-between gap-2">
                          <span className="text-muted-foreground">{name}</span>
                          <span className="font-medium tabular-nums text-foreground">
                            {item.dataKey === "cumulative_cost_cents" ? fmtCost(Number(value) / 100) : fmtNum(Number(value))}
                          </span>
                        </div>
                      )}
                    />
                  }
                />
                <ChartLegend content={<ChartLegendContent />} />
                <Bar yAxisId="left" dataKey="tokens" fill="var(--color-tokens)" maxBarSize={22} radius={[4, 4, 0, 0]} />
                <Line yAxisId="right" type="monotone" dataKey="cumulative_cost_cents" stroke="var(--color-cumulative_cost_cents)" strokeWidth={2} dot={false} />
              </ComposedChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle>Messages</CardTitle>
          <Button type="button" variant="outline" size="sm" onClick={handleExportMessages} disabled={paginatedMessages.length === 0}>
            <Download className="mr-1.5 h-3.5 w-3.5" aria-hidden="true" />
            Export CSV
          </Button>
        </CardHeader>
        <CardContent>
          <div className="overflow-hidden rounded-md border border-border bg-background p-1">
            <Table className="table-fixed">
              <TableHeader>
                <TableRow>
                  <SortableHead label="Time" column="timestamp" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <TableHead className="w-12">#</TableHead>
                  {showProviderColumn ? (
                    <SortableHead label="Agent" column="provider" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  ) : null}
                  <SortableHead label="Model" column="model" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Repo" column="repo_id" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Branch" column="git_branch" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <TableHead>Tools</TableHead>
                  <TableHead>Tags</TableHead>
                  <SortableHead label="Tokens" column="tokens" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                  <SortableHead label="Cost" column="cost" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                </TableRow>
              </TableHeader>
              <TableBody>
                {paginatedMessages.map((message, index) => {
                  const providerDisplay = providers.find((entry) => entry.name === message.provider)?.display_name ?? message.provider;
                  const rawModel = message.model ?? "";
                  const number = message.assistant_sequence ?? messageOffset + index + 1;
                  const repoLabel = repoName(message.repo_id);
                  const branchLabel = message.git_branch?.replace(/^refs\/heads\//, "") || "--";
                  const tagLabel = (message.tags ?? [])
                    .filter((tag) => tag.key !== "tool")
                    .map((tag) => `${tag.key}:${tag.value}`)
                    .slice(0, 2)
                    .join(", ");
                  const estimated =
                    (message.cost_confidence ?? "estimated") !== "otel_exact" &&
                    (message.cost_confidence ?? "estimated") !== "exact" &&
                    (message.cost_confidence ?? "estimated") !== "exact_cost";
                  return (
                    <TableRow key={message.id ?? `${message.timestamp}-${messageOffset + index}`}>
                      <TableCell className={MESSAGE_CELL_CLASS}>{fmtDate(message.timestamp)}</TableCell>
                      <TableCell className={`${MESSAGE_CELL_CLASS} w-12 whitespace-nowrap`}>#{number}</TableCell>
                      {showProviderColumn ? <TableCell className={MESSAGE_CELL_CLASS}>{providerDisplay}</TableCell> : null}
                      <TableCell className={MESSAGE_CELL_CLASS} title={rawModel}>
                        {formatModelName(rawModel)}
                      </TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={message.repo_id ?? ""}>{repoLabel}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={branchLabel}>{branchLabel}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={(message.tools ?? []).join(", ")}>
                        {compactTools(message.tools)}
                      </TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={tagLabel}>{tagLabel || "--"}</TableCell>
                      <TableCell className={`${MESSAGE_CELL_CLASS} whitespace-nowrap text-right`}>
                        {fmtNum((message.input_tokens ?? 0) + (message.output_tokens ?? 0))}
                      </TableCell>
                      <TableCell className={`${MESSAGE_CELL_CLASS} whitespace-nowrap text-right`}>
                        {(estimated ? "~" : "") + fmtCost((message.cost_cents ?? 0) / 100)}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </div>
          <div className="mt-3 flex items-center justify-between text-sm text-muted-foreground">
            <p>
              Showing {paginatedMessages.length === 0 ? 0 : messageOffset + 1}-{messageOffset + paginatedMessages.length} of {fmtNum(messageTotalCount)}
            </p>
            <div className="flex gap-2">
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={messageOffset === 0}
                onClick={() => setMessageOffset((previous) => Math.max(0, previous - MESSAGE_PAGE_SIZE))}
              >
                Previous
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={!hasMoreMessages}
                onClick={() => setMessageOffset((previous) => previous + MESSAGE_PAGE_SIZE)}
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
