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

export interface SessionHealthData {
  session_id: string;
  state: string;
  tip: string;
  message_count: number;
  total_cost_cents: number;
  vitals: Record<string, unknown>;
}

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

export function formatStatusText(data: StatuslineData): string {
  if (data.health_state && data.session_cost !== undefined) {
    const icon = healthIcon(data.health_state);
    const cost = formatCost(data.session_cost);
    const tip = data.health_tip ? ` · ${data.health_tip.toLowerCase()}` : "";
    return `${icon} budi · ${cost} session${tip}`;
  }

  if (data.today_cost > 0) {
    return `budi · ${formatCost(data.today_cost)} today`;
  }

  return "budi";
}

export function formatTooltip(data: StatuslineData): string {
  const lines: string[] = ["budi — AI cost tracker"];

  if (data.session_cost !== undefined) {
    lines.push(`Session: ${formatCost(data.session_cost)}`);
  }
  lines.push(`Today: ${formatCost(data.today_cost)}`);
  lines.push(`Week: ${formatCost(data.week_cost)}`);
  lines.push(`Month: ${formatCost(data.month_cost)}`);

  if (data.branch_cost !== undefined) {
    lines.push(`Branch: ${formatCost(data.branch_cost)}`);
  }
  if (data.active_provider) {
    lines.push(`Provider: ${data.active_provider}`);
  }
  if (data.health_state) {
    const label =
      data.health_state === "green"
        ? "healthy"
        : data.health_state === "gray"
          ? "not enough data yet"
          : data.health_tip || data.health_state;
    lines.push(`Health: ${label}`);
  }

  lines.push("", "Click to open dashboard");
  return lines.join("\n");
}

function healthIcon(state: string): string {
  switch (state) {
    case "red":
      return "\u{1F534}";
    case "yellow":
      return "\u{1F7E1}";
    case "gray":
      return "\u26AA";
    default:
      return "\u{1F7E2}";
  }
}

/**
 * Fetch statusline data by calling `budi statusline --format json`.
 * Falls back to a direct daemon HTTP call if the CLI is not available.
 */
export async function fetchStatusline(
  daemonUrl: string,
  sessionId?: string,
  cwd?: string
): Promise<StatuslineData | null> {
  const cliResult = await fetchViaCli(sessionId, cwd);
  if (cliResult) {
    return cliResult;
  }

  return fetchViaDaemon(daemonUrl, sessionId, cwd);
}

function fetchViaCli(
  sessionId?: string,
  cwd?: string
): Promise<StatuslineData | null> {
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
  cwd?: string
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
  sessionId?: string
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
