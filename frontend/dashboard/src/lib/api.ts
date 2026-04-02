import { granularityForPeriod, periodRange } from "@/lib/format";
import type {
  ActivityRow,
  BranchRow,
  CacheEfficiency,
  ConfidenceCostRow,
  CostSummary,
  DateRangeSelection,
  DaemonHealth,
  InstallIntegrationsRequest,
  IntegrationsHealth,
  MessageRow,
  ModelRow,
  ProjectRow,
  ProviderStats,
  RegisteredProvider,
  SchemaVersion,
  SessionCurveRow,
  SessionHealth,
  SessionsResponse,
  SessionTag,
  SubagentCostRow,
  Summary,
  SyncStatus,
  TagCostRow,
  ToolRow,
} from "@/lib/types";

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init);
  const payload = (await response.json().catch(() => ({}))) as T;
  if (!response.ok) {
    const message = (payload as { error?: string })?.error ?? `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return payload;
}

function withPeriod(
  path: string,
  period: DateRangeSelection,
  extra: Record<string, string | number | boolean | undefined> = {},
): string {
  const params = new URLSearchParams();
  const range = periodRange(period);

  if (range.since) params.set("since", range.since);
  if (range.until) params.set("until", range.until);

  for (const [key, value] of Object.entries(extra)) {
    if (value == null) continue;
    params.set(key, String(value));
  }

  const search = params.toString();
  return search ? `${path}?${search}` : path;
}

export async function fetchRegisteredProviders(signal?: AbortSignal): Promise<RegisteredProvider[]> {
  return fetchJson<RegisteredProvider[]>("/admin/providers", signal ? { signal } : undefined);
}

export async function fetchOverview(period: DateRangeSelection, signal?: AbortSignal) {
  const tzOffset = -new Date().getTimezoneOffset();

  const [summary, cost, projects, models, activity, providers, branches, tickets, activities] = await Promise.all([
    fetchJson<Summary>(withPeriod("/analytics/summary", period), signal ? { signal } : undefined),
    fetchJson<CostSummary>(withPeriod("/analytics/cost", period), signal ? { signal } : undefined),
    fetchJson<ProjectRow[]>(withPeriod("/analytics/projects", period, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ModelRow[]>(withPeriod("/analytics/models", period, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ActivityRow[]>(
      withPeriod("/analytics/activity", period, {
        granularity: granularityForPeriod(period),
        tz_offset: tzOffset,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<ProviderStats[]>(withPeriod("/analytics/providers", period), signal ? { signal } : undefined),
    fetchJson<BranchRow[]>(withPeriod("/analytics/branches", period, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<TagCostRow[]>(
      withPeriod("/analytics/tags", period, {
        key: "ticket_id",
        limit: 15,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<TagCostRow[]>(
      withPeriod("/analytics/tags", period, {
        key: "activity",
        limit: 15,
      }),
      signal ? { signal } : undefined,
    ),
  ]);

  return {
    summary,
    cost,
    projects,
    models,
    activity,
    providers,
    branches,
    tickets,
    activities,
  };
}

export async function fetchInsights(period: DateRangeSelection, signal?: AbortSignal) {
  const [cacheEff, sessionCurve, confidence, subagent, speedTags, tools, mcp] = await Promise.all([
    fetchJson<CacheEfficiency>(withPeriod("/analytics/cache-efficiency", period), signal ? { signal } : undefined),
    fetchJson<SessionCurveRow[]>(withPeriod("/analytics/session-cost-curve", period), signal ? { signal } : undefined),
    fetchJson<ConfidenceCostRow[]>(withPeriod("/analytics/cost-confidence", period), signal ? { signal } : undefined),
    fetchJson<SubagentCostRow[]>(withPeriod("/analytics/subagent-cost", period), signal ? { signal } : undefined),
    fetchJson<TagCostRow[]>(
      withPeriod("/analytics/tags", period, {
        key: "speed",
        limit: 10,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<ToolRow[]>(withPeriod("/analytics/tools", period, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ToolRow[]>(withPeriod("/analytics/mcp", period, { limit: 15 }), signal ? { signal } : undefined),
  ]);

  return { cacheEff, sessionCurve, confidence, subagent, speedTags, tools, mcp };
}

export interface SessionsQuery {
  limit?: number;
  offset?: number;
  search?: string;
  sort_by?: string;
  sort_asc?: boolean;
}

export async function fetchSessions(
  period: DateRangeSelection,
  query: SessionsQuery,
  signal?: AbortSignal,
): Promise<SessionsResponse> {
  return fetchJson<SessionsResponse>(
    withPeriod("/analytics/sessions", period, query as Record<string, string | number | boolean | undefined>),
    signal ? { signal } : undefined,
  );
}

export async function fetchSessionMessages(sessionId: string, signal?: AbortSignal): Promise<MessageRow[]> {
  return fetchJson<MessageRow[]>(`/analytics/sessions/${encodeURIComponent(sessionId)}/messages`, signal ? { signal } : undefined);
}

export async function fetchSessionTags(sessionId: string, signal?: AbortSignal): Promise<SessionTag[]> {
  return fetchJson<SessionTag[]>(`/analytics/sessions/${encodeURIComponent(sessionId)}/tags`, signal ? { signal } : undefined);
}

export async function fetchSessionHealth(sessionId: string, signal?: AbortSignal): Promise<SessionHealth | null> {
  return fetchJson<SessionHealth>(`/analytics/session-health?session_id=${encodeURIComponent(sessionId)}`, signal ? { signal } : undefined);
}

export async function fetchDaemonHealth(signal?: AbortSignal): Promise<DaemonHealth> {
  return fetchJson<DaemonHealth>("/health", signal ? { signal } : undefined);
}

export async function fetchSchemaVersion(signal?: AbortSignal): Promise<SchemaVersion> {
  return fetchJson<SchemaVersion>("/admin/schema", signal ? { signal } : undefined);
}

export async function fetchSyncStatus(signal?: AbortSignal): Promise<SyncStatus> {
  return fetchJson<SyncStatus>("/sync/status", signal ? { signal } : undefined);
}

export async function fetchIntegrations(signal?: AbortSignal): Promise<IntegrationsHealth> {
  return fetchJson<IntegrationsHealth>("/health/integrations", signal ? { signal } : undefined);
}

export async function fetchSettings(signal?: AbortSignal) {
  const [health, schema, syncStatus, integrations] = await Promise.all([
    fetchDaemonHealth(signal),
    fetchSchemaVersion(signal),
    fetchSyncStatus(signal),
    fetchIntegrations(signal),
  ]);

  return {
    health,
    schema,
    syncStatus,
    integrations,
  };
}

export async function postSyncRecent() {
  return fetchJson<Record<string, unknown>>("/sync", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ migrate: true }),
  });
}

export async function postSyncReset() {
  return fetchJson<Record<string, unknown>>("/sync/reset", {
    method: "POST",
  });
}

export async function postSyncAll() {
  return fetchJson<Record<string, unknown>>("/sync/all", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ migrate: true }),
  });
}

export async function postMigrate() {
  return fetchJson<Record<string, unknown>>("/admin/migrate", {
    method: "POST",
  });
}

export async function postRepair() {
  return fetchJson<Record<string, unknown>>("/admin/repair", {
    method: "POST",
  });
}

export async function postInstallIntegrations(request: InstallIntegrationsRequest) {
  return fetchJson<Record<string, unknown>>("/admin/integrations/install", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
}

export async function fetchUpdateCheck() {
  return fetchJson<Record<string, unknown>>("/health/check-update");
}
