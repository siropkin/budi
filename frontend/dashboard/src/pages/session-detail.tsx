import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import { ArrowDown, ArrowUp } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ChartContainer, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchRegisteredProviders, fetchSessionHealth, fetchSessionMessages, fetchSessionTags } from "@/lib/api";
import { fmtCost, fmtDate, fmtNum, formatModelName } from "@/lib/format";
import { cn } from "@/lib/utils";

function healthVariant(state: string): "default" | "warning" | "success" {
  if (state === "red") return "default";
  if (state === "yellow") return "warning";
  return "success";
}

const MESSAGE_CELL_CLASS = "align-top text-sm text-foreground whitespace-normal break-words";
type MessageSortColumn = "timestamp" | "provider" | "model" | "tokens" | "cost";

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

  if (!sessionId) {
    return <ErrorState error={new Error("Session ID is missing in route")} />;
  }

  if (messagesQuery.isPending || tagsQuery.isPending || healthQuery.isPending || providersQuery.isPending) {
    return <LoadingState label="Loading session detail..." />;
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

  const messages = messagesQuery.data;
  const tags = tagsQuery.data.filter((tag) => !["provider", "model", "repo", "machine", "cost_confidence"].includes(tag.key));
  const health = healthQuery.data;
  const providers = providersQuery.data;
  const vitals = Object.entries(health?.vitals ?? {}).filter(([, vital]) => vital != null);

  let tokenTotal = 0;
  let costTotalCents = 0;
  for (const message of messages) {
    tokenTotal += (message.input_tokens ?? 0) + (message.output_tokens ?? 0);
    costTotalCents += message.cost_cents ?? 0;
  }

  const tokenGrowth = messages.map((message, index) => ({
    label: `#${index + 1}`,
    input_tokens: message.input_tokens,
  }));

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
        <Link to="/sessions" className={cn(buttonVariants({ variant: "outline", size: "sm" }))}>
          ← Back to Sessions
        </Link>
        <p className="max-w-[760px] truncate text-sm text-muted-foreground">Session ID: {decodeURIComponent(sessionId)}</p>
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Usage</CardTitle>
          </CardHeader>
          <CardContent className="space-y-1 text-sm">
            <p>
              Messages: <span className="font-semibold text-foreground">{fmtNum(messages.length)}</span>
            </p>
            <p>
              Tokens: <span className="font-semibold text-foreground">{fmtNum(tokenTotal)}</span>
            </p>
            <p>
              Cost: <span className="font-semibold text-primary">{fmtCost(costTotalCents / 100)}</span>
            </p>
            {health?.tip ? (
              <p className="text-muted-foreground">
                Health tip: <span className="text-foreground">{health.tip}</span>
              </p>
            ) : null}
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
          <CardTitle>Input Token Growth</CardTitle>
        </CardHeader>
        <CardContent>
          <ChartContainer
            config={{
              input_tokens: {
                label: "Input tokens",
                color: "hsl(var(--chart-1))",
              },
            }}
          >
            <BarChart data={tokenGrowth} margin={{ left: 12, right: 8 }} accessibilityLayer>
              <CartesianGrid strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
              <XAxis dataKey="label" tickLine={false} axisLine={false} />
              <YAxis dataKey="input_tokens" tickFormatter={(value) => fmtNum(value)} tickLine={false} axisLine={false} />
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
              <Bar dataKey="input_tokens" fill="var(--color-input_tokens)" maxBarSize={28} radius={[4, 4, 0, 0]} />
            </BarChart>
          </ChartContainer>
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
                  <SortableHead label="Time" column="timestamp" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Agent" column="provider" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Model" column="model" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} />
                  <SortableHead label="Tokens" column="tokens" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                  <SortableHead label="Cost" column="cost" sortBy={sortBy} sortAsc={sortAsc} onSort={onSort} right />
                </TableRow>
              </TableHeader>
              <TableBody>
                {sortedMessages.map((message, index) => {
                  const providerDisplay = providers.find((entry) => entry.name === message.provider)?.display_name ?? message.provider;
                  const rawModel = message.model ?? "";
                  return (
                    <TableRow key={`${message.timestamp}-${index}`}>
                      <TableCell className={MESSAGE_CELL_CLASS}>{fmtDate(message.timestamp)}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS}>{providerDisplay}</TableCell>
                      <TableCell className={MESSAGE_CELL_CLASS} title={rawModel}>
                        {formatModelName(rawModel)}
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
        </CardContent>
      </Card>
    </div>
  );
}
