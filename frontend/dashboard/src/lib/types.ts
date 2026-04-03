export type DateRangePreset = "today" | "last_7_days" | "last_30_days" | "all";

export interface DateRangeSelection {
  preset: DateRangePreset;
}

export interface RegisteredProvider {
  name: string;
  display_name: string;
}

export interface Summary {
  total_messages: number;
  total_user_messages: number;
  total_assistant_messages: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_creation_tokens: number;
  total_cache_read_tokens: number;
}

export interface CostSummary {
  total_cost: number;
  input_cost: number;
  output_cost: number;
  cache_write_cost: number;
  cache_read_cost: number;
}

export interface CostRow {
  cost_cents: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
}

export interface ProjectRow extends CostRow {
  repo_id: string | null;
}

export interface BranchRow extends CostRow {
  repo_id: string | null;
  git_branch: string | null;
}

export interface TagCostRow extends CostRow {
  value: string;
}

export interface ModelRow extends CostRow {
  model: string;
  provider: string;
  message_count: number;
}

export interface ActivityRow {
  label: string;
  message_count: number;
  input_tokens: number;
  output_tokens: number;
  cost_cents: number;
  tool_call_count: number;
}

export interface ProviderStats {
  provider: string;
  input_tokens: number;
  output_tokens: number;
  total_cost_cents?: number;
  estimated_cost?: number;
}

export interface CacheEfficiency {
  cache_savings_cents: number;
  cache_hit_rate: number;
  total_cache_read_tokens: number;
  total_cache_creation_tokens: number;
}

export interface SessionCurveRow {
  bucket: string;
  avg_cost_per_message_cents: number;
  session_count: number;
  total_cost_cents: number;
}

export interface ConfidenceCostRow {
  confidence: string;
  cost_cents: number;
}

export interface SubagentCostRow {
  category: "main" | "subagent" | string;
  cost_cents: number;
}

export interface ToolRow {
  tool_name: string;
  call_count: number;
  cost_cents?: number;
}

export interface SessionRow {
  session_id: string;
  provider: string;
  title: string | null;
  started_at: string;
  ended_at: string | null;
  duration_ms: number | null;
  model: string;
  repo_id: string | null;
  git_branch: string | null;
  input_tokens: number;
  output_tokens: number;
  cost_cents: number;
}

export interface SessionsResponse {
  sessions: SessionRow[];
  total_count: number;
}

export interface MessageRow {
  timestamp: string;
  provider: string;
  model: string;
  input_tokens: number;
  output_tokens: number;
  cost_cents: number;
  cost_confidence?: string;
}

export interface SessionTag {
  key: string;
  value: string;
}

export interface HealthVital {
  state: "green" | "yellow" | "red";
  label: string;
}

export interface HealthDetail {
  vital: string;
  state: "green" | "yellow" | "red";
  tip: string;
  actions: string[];
}

export interface SessionHealth {
  state: "green" | "yellow" | "red";
  tip?: string;
  vitals?: Record<string, HealthVital>;
  details?: HealthDetail[];
}

export interface DaemonHealth {
  ok: boolean;
  version: string;
}

export interface SchemaVersion {
  current: number | string;
  target: number | string;
  needs_migration?: boolean;
}

export interface SyncStatus {
  syncing: boolean;
  last_synced?: string;
}

export interface IntegrationsHealth {
  [key: string]: unknown;
  database?: {
    size_mb?: number;
    records?: number;
    first_record?: string;
  };
  paths?: {
    database?: string;
    config?: string;
    claude_settings?: string;
    cursor_hooks?: string;
  };
}

export interface InstallIntegrationsRequest {
  components: string[];
  statusline_preset?: string;
}
