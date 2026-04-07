import { granularityForPeriod, periodRange } from "@/lib/format";
import type {
  ActivityRow,
  BranchRow,
  CacheEfficiency,
  ConfidenceCostRow,
  CostSummary,
  DashboardFilters,
  DateRangeSelection,
  DaemonHealth,
  FilterOptionsResponse,
  InstallIntegrationsRequest,
  IntegrationsHealth,
  MessageRow,
  MessagesResponse,
  ModelRow,
  OtelEventRow,
  ProjectRow,
  ProviderStats,
  RegisteredProvider,
  RepairResponse,
  SessionHookEventRow,
  SchemaVersion,
  SessionDetailRow,
  SessionCurveRow,
  SessionHealth,
  SessionMessageCurvePoint,
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

function withFilters(
  path: string,
  filters: DashboardFilters,
  extra: Record<string, string | number | boolean | undefined> = {},
): string {
  const params = new URLSearchParams();
  const range = periodRange(filters.period);

  if (range.since) params.set("since", range.since);
  if (range.until) params.set("until", range.until);
  if (filters.agents.length > 0) params.set("agents", filters.agents.join(","));
  if (filters.models.length > 0) params.set("models", filters.models.join(","));
  if (filters.projects.length > 0) params.set("projects", filters.projects.join(","));
  if (filters.branches.length > 0) params.set("branches", filters.branches.join(","));

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

export async function fetchOverview(filters: DashboardFilters, signal?: AbortSignal) {
  const tzOffset = -new Date().getTimezoneOffset();

  const [summary, cost, projects, models, activity, providers, branches, tickets, activities] = await Promise.all([
    fetchJson<Summary>(withFilters("/analytics/summary", filters), signal ? { signal } : undefined),
    fetchJson<CostSummary>(withFilters("/analytics/cost", filters), signal ? { signal } : undefined),
    fetchJson<ProjectRow[]>(withFilters("/analytics/projects", filters, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ModelRow[]>(withFilters("/analytics/models", filters, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ActivityRow[]>(
      withFilters("/analytics/activity", filters, {
        granularity: granularityForPeriod(filters.period),
        tz_offset: tzOffset,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<ProviderStats[]>(withFilters("/analytics/providers", filters), signal ? { signal } : undefined),
    fetchJson<BranchRow[]>(withFilters("/analytics/branches", filters, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<TagCostRow[]>(
      withFilters("/analytics/tags", filters, {
        key: "ticket_id",
        limit: 15,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<TagCostRow[]>(
      withFilters("/analytics/tags", filters, {
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

export async function fetchInsights(filters: DashboardFilters, signal?: AbortSignal) {
  const [cacheEff, sessionCurve, confidence, subagent, speedTags, tools, mcp] = await Promise.all([
    fetchJson<CacheEfficiency>(withFilters("/analytics/cache-efficiency", filters), signal ? { signal } : undefined),
    fetchJson<SessionCurveRow[]>(withFilters("/analytics/session-cost-curve", filters), signal ? { signal } : undefined),
    fetchJson<ConfidenceCostRow[]>(withFilters("/analytics/cost-confidence", filters), signal ? { signal } : undefined),
    fetchJson<SubagentCostRow[]>(withFilters("/analytics/subagent-cost", filters), signal ? { signal } : undefined),
    fetchJson<TagCostRow[]>(
      withFilters("/analytics/tags", filters, {
        key: "speed",
        limit: 10,
      }),
      signal ? { signal } : undefined,
    ),
    fetchJson<ToolRow[]>(withFilters("/analytics/tools", filters, { limit: 15 }), signal ? { signal } : undefined),
    fetchJson<ToolRow[]>(withFilters("/analytics/mcp", filters, { limit: 15 }), signal ? { signal } : undefined),
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
  filters: DashboardFilters,
  query: SessionsQuery,
  signal?: AbortSignal,
): Promise<SessionsResponse> {
  return fetchJson<SessionsResponse>(
    withFilters("/analytics/sessions", filters, query as Record<string, string | number | boolean | undefined>),
    signal ? { signal } : undefined,
  );
}

const EXPORT_PAGE_SIZE = 200;

export async function fetchAllSessions(
  filters: DashboardFilters,
  search?: string,
  signal?: AbortSignal,
): Promise<SessionsResponse["sessions"]> {
  const all: SessionsResponse["sessions"] = [];
  let offset = 0;

  for (;;) {
    const page = await fetchSessions(
      filters,
      { limit: EXPORT_PAGE_SIZE, offset, search, sort_by: "started_at", sort_asc: false },
      signal,
    );
    all.push(...page.sessions);
    if (all.length >= page.total_count || page.sessions.length < EXPORT_PAGE_SIZE) break;
    offset += EXPORT_PAGE_SIZE;
  }

  return all;
}

export async function fetchAllSessionMessages(
  sessionId: string,
  signal?: AbortSignal,
): Promise<MessagesResponse["messages"]> {
  const all: MessagesResponse["messages"] = [];
  let offset = 0;

  for (;;) {
    const page = await fetchSessionMessagesWithRoles(
      sessionId,
      "assistant",
      { limit: EXPORT_PAGE_SIZE, offset, sort_by: "timestamp", sort_asc: true },
      signal,
    );
    all.push(...page.messages);
    if (all.length >= page.total_count || page.messages.length < EXPORT_PAGE_SIZE) break;
    offset += EXPORT_PAGE_SIZE;
  }

  return all;
}

export async function fetchFilterOptions(signal?: AbortSignal): Promise<FilterOptionsResponse> {
  return fetchJson<FilterOptionsResponse>("/analytics/filter-options", signal ? { signal } : undefined);
}

export async function fetchSessionMessages(sessionId: string, signal?: AbortSignal): Promise<MessageRow[]> {
  const response = await fetchSessionMessagesWithRoles(sessionId, "assistant", {}, signal);
  return response.messages;
}

export async function fetchSessionMessageCurve(sessionId: string, signal?: AbortSignal): Promise<SessionMessageCurvePoint[]> {
  return fetchJson<SessionMessageCurvePoint[]>(
    `/analytics/sessions/${encodeURIComponent(sessionId)}/curve`,
    signal ? { signal } : undefined,
  );
}

export interface SessionMessagesQuery {
  limit?: number;
  offset?: number;
  sort_by?: string;
  sort_asc?: boolean;
}

export async function fetchSessionMessagesWithRoles(
  sessionId: string,
  roles: "assistant" | "all",
  query: SessionMessagesQuery = {},
  signal?: AbortSignal,
): Promise<MessagesResponse> {
  const params = new URLSearchParams();
  params.set("roles", roles);
  if (query.limit != null) params.set("limit", String(query.limit));
  if (query.offset != null) params.set("offset", String(query.offset));
  if (query.sort_by) params.set("sort_by", query.sort_by);
  if (query.sort_asc != null) params.set("sort_asc", String(query.sort_asc));
  return fetchJson<MessagesResponse>(
    `/analytics/sessions/${encodeURIComponent(sessionId)}/messages?${params.toString()}`,
    signal ? { signal } : undefined,
  );
}

export async function fetchSessionDetail(sessionId: string, signal?: AbortSignal): Promise<SessionDetailRow> {
  return fetchJson<SessionDetailRow>(`/analytics/sessions/${encodeURIComponent(sessionId)}`, signal ? { signal } : undefined);
}

export async function fetchSessionTags(sessionId: string, signal?: AbortSignal): Promise<SessionTag[]> {
  return fetchJson<SessionTag[]>(`/analytics/sessions/${encodeURIComponent(sessionId)}/tags`, signal ? { signal } : undefined);
}

export async function fetchSessionHealth(sessionId: string, signal?: AbortSignal): Promise<SessionHealth | null> {
  return fetchJson<SessionHealth>(`/analytics/session-health?session_id=${encodeURIComponent(sessionId)}`, signal ? { signal } : undefined);
}

export async function fetchSessionHookEvents(
  sessionId: string,
  query: {
    linked_only?: boolean;
    event?: string;
    limit?: number;
    offset?: number;
    include_raw?: boolean;
  },
  signal?: AbortSignal,
): Promise<SessionHookEventRow[]> {
  const params = new URLSearchParams();
  if (query.linked_only != null) params.set("linked_only", String(query.linked_only));
  if (query.event) params.set("event", query.event);
  if (query.limit != null) params.set("limit", String(query.limit));
  if (query.offset != null) params.set("offset", String(query.offset));
  if (query.include_raw != null) params.set("include_raw", String(query.include_raw));
  const search = params.toString();
  const url = `/analytics/sessions/${encodeURIComponent(sessionId)}/hook-events${search ? `?${search}` : ""}`;
  return fetchJson<SessionHookEventRow[]>(url, signal ? { signal } : undefined);
}

export async function fetchSessionOtelEvents(
  sessionId: string,
  query: {
    linked_only?: boolean;
    limit?: number;
    offset?: number;
    include_raw?: boolean;
  },
  signal?: AbortSignal,
): Promise<OtelEventRow[]> {
  const params = new URLSearchParams();
  if (query.linked_only != null) params.set("linked_only", String(query.linked_only));
  if (query.limit != null) params.set("limit", String(query.limit));
  if (query.offset != null) params.set("offset", String(query.offset));
  if (query.include_raw != null) params.set("include_raw", String(query.include_raw));
  const search = params.toString();
  const url = `/analytics/sessions/${encodeURIComponent(sessionId)}/otel-events${search ? `?${search}` : ""}`;
  return fetchJson<OtelEventRow[]>(url, signal ? { signal } : undefined);
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
  return fetchJson<RepairResponse>("/admin/repair", {
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
