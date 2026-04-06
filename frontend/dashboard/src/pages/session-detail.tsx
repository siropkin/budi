import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useParams } from "react-router-dom";
import { Bar, CartesianGrid, ComposedChart, Line, XAxis, YAxis } from "recharts";
import { ArrowDown, ArrowUp } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ChartContainer, ChartLegend, ChartLegendContent, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { ErrorState, LoadingState } from "@/components/state";
import {
  fetchRegisteredProviders,
  fetchSessionDetail,
  fetchSessionHealth,
  fetchSessionMessages,
  fetchSessionTags,
} from "@/lib/api";
import { fmtCost, fmtDate, fmtNum, formatModelName, repoName } from "@/lib/format";
import { cn } from "@/lib/utils";

function healthVariant(state: string): "default" | "warning" | "success" {
  if (state === "red") return "default";
  if (state === "yellow") return "warning";
  return "success";
}

const MESSAGE_CELL_CLASS = "align-top text-sm text-foreground whitespace-normal break-words";
const MESSAGE_PAGE_SIZE = 50;
type MessageSortColumn = "timestamp" | "provider" | "model" | "tokens" | "cost";

function compactTools(tools: string[] | undefined, max = 2): string {
  const values = (tools ?? []).filter(Boolean);
  if (values.length === 0) return "--";
  if (values.length <= max) return values.join(", ");
  return `${values.slice(0, max).join(", ")} +${values.length - max}`;
}

function sourceLabel(confidence: string | undefined): string {
  const value = (confidence ?? "estimated").toLowerCase();
  if (value === "otel_exact") return "OTEL exact";
  if (value === "exact" || value === "exact_cost") return "exact";
  if (value === "estimated_unknown_model") return "estimated (model)";
  return value;
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
        className={cn("h-8 px-1 text-muted-foreground hover:text-foreground", isActive && "text-foreground", right && "ml-auto")}
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
  const sessionId = params.sessionId;
  const [sortBy, setSortBy] = useState<MessageSortColumn>("timestamp");
  const [sortAsc, setSortAsc] = useState(false);
  const [messageOffset, setMessageOffset] = useState(0);

  const detailQuery = useQuery({
    queryKey: ["session-detail", sessionId],
    queryFn: ({ signal }) => fetchSessionDetail(sessionId ?? "", signal),
    enabled: Boolean(sessionId),
  });

  const messagesQuery = useQuery({
    queryKey: ["session-messages", sessionId],
    queryFn: ({ signal }) => fetchSessionMessages(sessionId ?? "", signal),
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

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const messages = messagesQuery.data ?? [];
  const assistantMessages = messages;
  const providers = providersQuery.data ?? [];
  const sortedMessages = [...messages];
  sortedMessages.sort((left, right) => {
    let compare = 0;
    if (sortBy === "timestamp") {
      compare = left.timestamp.localeCompare(right.timestamp);
    } else if (sortBy === "provider") {
      const leftProvider = providers.find((entry) => entry.name === left.provider)?.display_name ?? left.provider;
      const rightProvider = providers.find((entry) => entry.name === right.provider)?.display_name ?? right.provider;
      compare = leftProvider.localeCompare(rightProvider);
    } else if (sortBy === "model") {
      compare = formatModelName(left.model ?? "").localeCompare(formatModelName(right.model ?? ""));
    } else if (sortBy === "tokens") {
      const leftTokens = (left.input_tokens ?? 0) + (left.output_tokens ?? 0);
      const rightTokens = (right.input_tokens ?? 0) + (right.output_tokens ?? 0);
      compare = leftTokens - rightTokens;
    } else if (sortBy === "cost") {
      compare = (left.cost_cents ?? 0) - (right.cost_cents ?? 0);
    }
    return sortAsc ? compare : -compare;
  });

  useEffect(() => {
    setMessageOffset(0);
  }, [sortBy, sortAsc, sessionId]);

  useEffect(() => {
    if (messageOffset >= sortedMessages.length && messageOffset > 0) {
      setMessageOffset(Math.max(0, sortedMessages.length - MESSAGE_PAGE_SIZE));
    }
  }, [messageOffset, sortedMessages.length]);

  if (!sessionId) {
    return <ErrorState error={new Error("Session ID is missing in route")} />;
  }

  if (
    messagesQuery.isPending ||
    tagsQuery.isPending ||
    healthQuery.isPending ||
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

  if (providersQuery.error) {
    return <ErrorState error={providersQuery.error} onRetry={() => providersQuery.refetch()} />;
  }

  const sessionDetail = detailQuery.data;
  if (!sessionDetail) {
    return <ErrorState error={new Error("Session detail is unavailable")} />;
  }

  const tags = tagsQuery.data ?? [];
  const health = healthQuery.data;
  const vitals = Object.entries(health?.vitals ?? {}).filter(([, vital]) => vital != null);

  let tokenTotal = 0;
  let costTotalCents = 0;
  for (const message of assistantMessages) {
    tokenTotal += (message.input_tokens ?? 0) + (message.output_tokens ?? 0);
    costTotalCents += message.cost_cents ?? 0;
  }

  const sessionCurve = [];
  let cumulativeCostCents = 0;
  for (let index = 0; index < assistantMessages.length; index += 1) {
    const message = assistantMessages[index];
    cumulativeCostCents += message.cost_cents ?? 0;
    sessionCurve.push({
      label: `#${index + 1}`,
      tokens: (message.input_tokens ?? 0) + (message.output_tokens ?? 0),
      cumulative_cost_cents: cumulativeCostCents,
    });
  }

  const paginatedMessages = sortedMessages.slice(messageOffset, messageOffset + MESSAGE_PAGE_SIZE);
  const hasMoreMessages = messageOffset + paginatedMessages.length < sortedMessages.length;
  const messageNumberById = new Map<string, number>();
  for (let index = 0; index < assistantMessages.length; index += 1) {
    const uuid = assistantMessages[index].uuid;
    if (uuid) {
      messageNumberById.set(uuid, index + 1);
    }
  }

  const overviewRepoIds =
    sessionDetail.repo_ids?.filter((repo) => repo && repo !== "unknown") ??
    [];
  const overviewRepoPrimary =
    overviewRepoIds[0] ??
    sessionDetail.repo_id ??
    messages.find((message) => message.repo_id && message.repo_id !== "unknown")?.repo_id ??
    null;
  const overviewRepoCount = sessionDetail.repo_count ?? overviewRepoIds.length;
  const overviewRepoLabel =
    overviewRepoCount > 1
      ? `${repoName(overviewRepoPrimary)} +${overviewRepoCount - 1}`
      : repoName(overviewRepoPrimary);

  const overviewBranches =
    sessionDetail.git_branches?.filter((branch) => branch && branch !== "") ??
    [];
  const rawOverviewBranch =
    overviewBranches[0] ??
    sessionDetail.git_branch ??
    messages.find((message) => message.git_branch && message.git_branch !== "")?.git_branch ??
    null;
  const overviewBranchPrimary = rawOverviewBranch?.replace(/^refs\/heads\//, "") ?? "--";
  const overviewBranchCount = sessionDetail.git_branch_count ?? overviewBranches.length;
  const overviewBranchLabel =
    overviewBranchCount > 1
      ? `${overviewBranchPrimary} +${overviewBranchCount - 1}`
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

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-semibold tracking-tight text-foreground">{overviewName}</h2>
          <p className="font-mono text-xs text-muted-foreground">{decodeURIComponent(sessionId)}</p>
        </div>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Overview</CardTitle>
          </CardHeader>
          <CardContent className="space-y-1 text-sm">
            <p>
              Session: <span className="font-semibold text-foreground">{overviewName}</span>
            </p>
            <p>
              Cost: <span className="font-semibold text-primary">{fmtCost(costTotalCents / 100)}</span>
            </p>
            <p>
              Tokens: <span className="font-semibold text-foreground">{fmtNum(tokenTotal)}</span>
            </p>
            <p>
              Messages: <span className="font-semibold text-foreground">{fmtNum(assistantMessages.length)}</span>
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
            <CardTitle>Health Vitals</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-2">
            {vitals.map(([key, vital]) => (
              <div key={key} className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2">
                <span className="text-sm text-muted-foreground">{key.replace(/_/g, " ")}</span>
                <Badge variant={healthVariant(vital.state)}>{vital.label}</Badge>
              </div>
            ))}
            {vitals.length === 0 ? (
              <p className="text-sm text-muted-foreground">No session-health vitals available.</p>
            ) : null}
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
        <CardHeader>
          <CardTitle>Messages</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="overflow-hidden rounded-md border border-border bg-background p-1">
            <Table className="table-fixed">
              <TableHeader>
                <TableRow>
                  <TableHead>#</TableHead>
                  <SortableHead label="Time" column="timestamp" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Agent" column="provider" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Model" column="model" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <TableHead>Tools</TableHead>
                  <TableHead>Source</TableHead>
                  <SortableHead label="Tokens" column="tokens" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                  <SortableHead label="Cost" column="cost" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                </TableRow>
              </TableHeader>
              <TableBody>
                {paginatedMessages.map((message, index) => {
                  const providerDisplay = providers.find((entry) => entry.name === message.provider)?.display_name ?? message.provider;
                  const rawModel = message.model ?? "";
                  const number = message.uuid ? messageNumberById.get(message.uuid) : undefined;
                  return (
                    <TableRow key={message.uuid ?? `${message.timestamp}-${messageOffset + index}`}>
                      <TableCell className={`${MESSAGE_CELL_CLASS} whitespace-nowrap`}>{number != null ? `#${number}` : "--"}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS}>{fmtDate(message.timestamp)}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS}>{providerDisplay}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={rawModel}>
                        {formatModelName(rawModel)}
                      </TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={(message.tools ?? []).join(", ")}>
                        {compactTools(message.tools)}
                      </TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS}>
                        <Badge variant={message.cost_confidence === "otel_exact" ? "success" : "outline"}>
                          {sourceLabel(message.cost_confidence)}
                        </Badge>
                      </TableCell>
                      <TableCell className={`${MESSAGE_CELL_CLASS} whitespace-nowrap text-right`}>
                        {fmtNum((message.input_tokens ?? 0) + (message.output_tokens ?? 0))}
                      </TableCell>
                      <TableCell className={`${MESSAGE_CELL_CLASS} whitespace-nowrap text-right`}>
                        {fmtCost((message.cost_cents ?? 0) / 100)}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </div>
          <div className="mt-3 flex items-center justify-between text-sm text-muted-foreground">
            <p>
              Showing {paginatedMessages.length === 0 ? 0 : messageOffset + 1}-{messageOffset + paginatedMessages.length} of {fmtNum(sortedMessages.length)}
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
