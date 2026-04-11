import { spawn } from "child_process";
import * as http from "http";

export interface StatuslineData {
  today_cost: number;
  week_cost: number;
  month_cost: number;
  session_cost?: number;
  branch_cost?: number;
  project_cost?: number;
  active_provider?: string;
  health_state?: string;
  health_tip?: string;
  session_msg_cost?: number;
}

export interface VitalScore {
  state: string;
  label: string;
}

export interface SessionVitals {
  context_drag?: VitalScore;
  cache_efficiency?: VitalScore;
  thrashing?: VitalScore;
  cost_acceleration?: VitalScore;
}

export interface SessionHealthData {
  session_id: string;
  state: string;
  tip: string;
  message_count: number;
  total_cost_cents: number;
  vitals: SessionVitals;
}

export interface SessionListEntry {
  session_id: string;
  started_at?: string;
  ended_at?: string;
  message_count: number;
  cost_cents: number;
  model?: string;
  provider: string;
  health_state?: string;
  title?: string;
}

export interface HealthAggregation {
  green: number;
  yellow: number;
  red: number;
  total: number;
}

export interface DaemonHealth {
  ok: boolean;
  version: string;
  api_version: number;
}

/** The minimum daemon api_version this extension requires. */
export const MIN_API_VERSION = 1;

function formatCost(dollars: number): string {
  if (dollars >= 1000) {
    return `$${(dollars / 1000).toFixed(1)}K`;
  }
  if (dollars >= 100) {
    return `$${Math.round(dollars)}`;
  }
  if (dollars > 0) {
    return `$${dollars.toFixed(2)}`;
  }
  return "$0.00";
}

export function formatAggregationStatusText(agg: HealthAggregation): string {
  if (agg.total === 0) return "budi";
  return `budi · \u{1F7E2} ${agg.green} \u{1F7E1} ${agg.yellow} \u{1F534} ${agg.red}`;
}

export function formatAggregationTooltip(agg: HealthAggregation, todayCost: number): string {
  const lines: string[] = ["budi — AI cost tracker", ""];
  lines.push(`Today's sessions: ${agg.total}`);
  if (agg.green > 0) lines.push(`  \u{1F7E2} ${agg.green} healthy`);
  if (agg.yellow > 0) lines.push(`  \u{1F7E1} ${agg.yellow} warning`);
  if (agg.red > 0) lines.push(`  \u{1F534} ${agg.red} needs attention`);
  lines.push("");
  lines.push(`Today: ${formatCost(todayCost)}`);
  lines.push("", "Click to open session health");
  return lines.join("\n");
}

export function aggregateHealth(sessions: SessionListEntry[]): HealthAggregation {
  const agg: HealthAggregation = { green: 0, yellow: 0, red: 0, total: 0 };
  for (const s of sessions) {
    agg.total++;
    switch (s.health_state) {
      case "yellow":
        agg.yellow++;
        break;
      case "red":
        agg.red++;
        break;
      default:
        agg.green++;
        break;
    }
  }
  return agg;
}

/**
 * Check daemon health and return version / api_version info.
 * Returns null if the daemon is unreachable.
 */
export function fetchDaemonHealth(daemonUrl: string): Promise<DaemonHealth | null> {
  return new Promise((resolve) => {
    const req = http.get(`${daemonUrl}/health`, { timeout: 3000 }, (res) => {
      let body = "";
      res.on("data", (chunk: Buffer) => {
        body += chunk.toString();
      });
      res.on("end", () => {
        try {
          resolve(JSON.parse(body));
        } catch {
          resolve(null);
        }
      });
    });

    req.on("error", () => resolve(null));
    req.on("timeout", () => {
      req.destroy();
      resolve(null);
    });
  });
}

/**
 * Fetch statusline data by calling `budi statusline --format json`.
 * Falls back to a direct daemon HTTP call if the CLI is not available.
 */
export async function fetchStatusline(
  daemonUrl: string,
  sessionId?: string,
  cwd?: string,
): Promise<StatuslineData | null> {
  const cliResult = await fetchViaCli(sessionId, cwd);
  if (cliResult) {
    return cliResult;
  }

  return fetchViaDaemon(daemonUrl, sessionId, cwd);
}

function fetchViaCli(sessionId?: string, cwd?: string): Promise<StatuslineData | null> {
  return new Promise((resolve) => {
    const child = spawn("budi", ["statusline", "--format", "json"], {
      stdio: ["pipe", "pipe", "ignore"],
      timeout: 3000,
    });

    let stdout = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
    });

    child.on("error", () => resolve(null));
    child.on("close", () => {
      const trimmed = stdout.trim();
      if (!trimmed) {
        resolve(null);
        return;
      }
      try {
        resolve(JSON.parse(trimmed));
      } catch {
        resolve(null);
      }
    });

    const input: Record<string, string> = {};
    if (sessionId) {
      input.session_id = sessionId;
    }
    if (cwd) {
      input.cwd = cwd;
    }
    child.stdin.write(JSON.stringify(input));
    child.stdin.end();
  });
}

function fetchViaDaemon(
  baseUrl: string,
  sessionId?: string,
  cwd?: string,
): Promise<StatuslineData | null> {
  return new Promise((resolve) => {
    const url = new URL("/analytics/statusline", baseUrl);
    if (sessionId) {
      url.searchParams.set("session_id", sessionId);
    }
    if (cwd) {
      url.searchParams.set("project_dir", cwd);
    }

    const req = http.get(url.toString(), { timeout: 3000 }, (res) => {
      let body = "";
      res.on("data", (chunk: Buffer) => {
        body += chunk.toString();
      });
      res.on("end", () => {
        try {
          resolve(JSON.parse(body));
        } catch {
          resolve(null);
        }
      });
    });

    req.on("error", () => resolve(null));
    req.on("timeout", () => {
      req.destroy();
      resolve(null);
    });
  });
}

export function fetchSessionHealth(
  daemonUrl: string,
  sessionId?: string,
): Promise<SessionHealthData | null> {
  return new Promise((resolve) => {
    const url = new URL("/analytics/session-health", daemonUrl);
    if (sessionId) {
      url.searchParams.set("session_id", sessionId);
    }

    const req = http.get(url.toString(), { timeout: 3000 }, (res) => {
      let body = "";
      res.on("data", (chunk: Buffer) => {
        body += chunk.toString();
      });
      res.on("end", () => {
        try {
          resolve(JSON.parse(body));
        } catch {
          resolve(null);
        }
      });
    });

    req.on("error", () => resolve(null));
    req.on("timeout", () => {
      req.destroy();
      resolve(null);
    });
  });
}

/**
 * Fetch recent sessions (today + yesterday) with health state from the daemon.
 */
export function fetchRecentSessions(daemonUrl: string): Promise<SessionListEntry[] | null> {
  return new Promise((resolve) => {
    const yesterday = new Date();
    yesterday.setDate(yesterday.getDate() - 1);
    yesterday.setHours(0, 0, 0, 0);
    const since = yesterday.toISOString();

    const url = new URL("/analytics/sessions", daemonUrl);
    url.searchParams.set("since", since);
    url.searchParams.set("limit", "100");
    url.searchParams.set("sort_by", "started_at");
    url.searchParams.set("sort_asc", "false");

    const req = http.get(url.toString(), { timeout: 3000 }, (res) => {
      let body = "";
      res.on("data", (chunk: Buffer) => {
        body += chunk.toString();
      });
      res.on("end", () => {
        try {
          const parsed = JSON.parse(body);
          resolve(parsed.sessions ?? parsed);
        } catch {
          resolve(null);
        }
      });
    });

    req.on("error", () => resolve(null));
    req.on("timeout", () => {
      req.destroy();
      resolve(null);
    });
  });
}

export function splitSessionsByDay(sessions: SessionListEntry[]): {
  today: SessionListEntry[];
  yesterday: SessionListEntry[];
} {
  const now = new Date();
  const todayStart = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const yesterdayStart = new Date(todayStart);
  yesterdayStart.setDate(yesterdayStart.getDate() - 1);

  const today: SessionListEntry[] = [];
  const yesterday: SessionListEntry[] = [];

  for (const s of sessions) {
    const d = s.started_at ? new Date(s.started_at) : null;
    if (!d) continue;
    if (d >= todayStart) {
      today.push(s);
    } else if (d >= yesterdayStart) {
      yesterday.push(s);
    }
  }

  return { today, yesterday };
}
