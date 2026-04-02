import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { Bar, BarChart, CartesianGrid, Tooltip, XAxis, YAxis } from "recharts";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ChartContainer } from "@/components/ui/chart";
import { ErrorState, LoadingState } from "@/components/state";
import { fetchSessionHealth, fetchSessionMessages, fetchSessionTags } from "@/lib/api";
import { fmtCost, fmtDate, fmtNum, formatModelName } from "@/lib/format";

function healthVariant(state: string): "default" | "warning" | "success" {
  if (state === "red") return "default";
  if (state === "yellow") return "warning";
  return "success";
}

export function SessionDetailPage() {
  const params = useParams<{ sessionId: string }>();
  const sessionId = params.sessionId;

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

  if (!sessionId) {
    return <ErrorState error={new Error("Session ID is missing in route")} />;
  }

  if (messagesQuery.isPending || tagsQuery.isPending || healthQuery.isPending) {
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

  const messages = messagesQuery.data;
  const tags = tagsQuery.data.filter((tag) => !["provider", "model", "repo", "machine", "cost_confidence"].includes(tag.key));
  const health = healthQuery.data;

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

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Link to="/sessions" className="rounded-md border border-border px-3 py-2 text-sm hover:bg-muted">
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
            {Object.entries(health?.vitals ?? {}).map(([key, vital]) => (
              <div key={key} className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2">
                <span className="text-sm text-muted-foreground">{key.replace(/_/g, " ")}</span>
                <Badge variant={healthVariant(vital.state)}>{vital.label}</Badge>
              </div>
            ))}
            {Object.keys(health?.vitals ?? {}).length === 0 ? (
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
              input: {
                label: "Input tokens",
                color: "hsl(var(--chart-1))",
              },
            }}
          >
            <BarChart data={tokenGrowth} margin={{ left: 12, right: 8 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="rgba(255,255,255,0.08)" />
              <XAxis dataKey="label" tickLine={false} axisLine={false} />
              <YAxis dataKey="input_tokens" tickFormatter={(value) => fmtNum(value)} tickLine={false} axisLine={false} />
              <Tooltip
                content={({ active, payload, label }) => {
                  if (!active || !payload || payload.length === 0) return null;
                  const value = Number(payload[0].value ?? 0);
                  return (
                    <div className="rounded-md border border-border bg-card px-3 py-2 text-xs shadow-md">
                      <p className="font-medium">{label}</p>
                      <p className="text-muted-foreground">Input: {fmtNum(value)}</p>
                    </div>
                  );
                }}
              />
              <Bar dataKey="input_tokens" fill="var(--color-input)" radius={[4, 4, 0, 0]} />
            </BarChart>
          </ChartContainer>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Messages</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="overflow-x-auto rounded-md border border-border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Time</TableHead>
                  <TableHead>Agent</TableHead>
                  <TableHead>Model</TableHead>
                  <TableHead className="text-right">Tokens</TableHead>
                  <TableHead className="text-right">Cost</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {messages.map((message, index) => {
                  const isExact =
                    !message.cost_confidence ||
                    message.cost_confidence === "exact" ||
                    message.cost_confidence === "exact_cost" ||
                    message.cost_confidence === "otel_exact";
                  return (
                    <TableRow key={`${message.timestamp}-${index}`}>
                      <TableCell className="text-muted-foreground">{fmtDate(message.timestamp)}</TableCell>
                      <TableCell className="text-muted-foreground">{message.provider}</TableCell>
                      <TableCell>{formatModelName(message.model)}</TableCell>
                      <TableCell className="text-right font-mono text-muted-foreground">
                        {fmtNum((message.input_tokens ?? 0) + (message.output_tokens ?? 0))}
                      </TableCell>
                      <TableCell className="text-right font-mono">
                        {isExact ? "" : "~"}
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
