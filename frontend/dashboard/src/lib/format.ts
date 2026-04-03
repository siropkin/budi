import type { DateRangeSelection } from "@/lib/types";

const DAY_MS = 86_400_000;

function startOfDay(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate());
}

function addDays(date: Date, days: number): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate() + days);
}

export function periodLabel(period: DateRangeSelection): string {
  switch (period.preset) {
    case "today":
      return "Today";
    case "last_7_days":
      return "Last 7 days";
    case "last_30_days":
      return "Last 30 days";
    case "all":
      return "All";
    default:
      return "Today";
  }
}

export function periodRange(period: DateRangeSelection): { since?: string; until?: string } {
  const now = new Date();
  const today = startOfDay(now);
  const toIso = (value: Date) => value.toISOString();

  switch (period.preset) {
    case "today":
      return { since: toIso(today), until: toIso(addDays(today, 1)) };
    case "last_7_days":
      return { since: toIso(addDays(today, -6)), until: toIso(addDays(today, 1)) };
    case "last_30_days":
      return { since: toIso(addDays(today, -29)), until: toIso(addDays(today, 1)) };
    case "all":
      return {};
    default:
      return { since: toIso(today), until: toIso(addDays(today, 1)) };
  }
}

export function granularityForPeriod(period: DateRangeSelection): "hour" | "day" | "month" {
  if (period.preset === "today") {
    return "hour";
  }
  if (period.preset === "all") {
    return "month";
  }

  const range = periodRange(period);
  if (!range.since || !range.until) {
    return "day";
  }

  const sinceTime = new Date(range.since).getTime();
  const untilTime = new Date(range.until).getTime();
  const days = Math.max(1, Math.round((untilTime - sinceTime) / DAY_MS));

  if (days <= 2) return "hour";
  if (days > 120) return "month";
  return "day";
}

export function fmtNum(value: number): string {
  if (value >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(1)}B`;
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(value);
}

export function fmtCost(value: number): string {
  if (value < 0) return `-${fmtCost(-value)}`;
  if (value >= 1000) return `$${(value / 1000).toFixed(1)}K`;
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value > 0) return `$${value.toFixed(2)}`;
  return "$0.00";
}

export function fmtDate(iso: string | null | undefined): string {
  if (!iso) return "--";

  const date = new Date(iso);
  const now = new Date();
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const target = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const diffDays = Math.floor((today.getTime() - target.getTime()) / 86_400_000);
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

  if (diffDays === 0) return `Today ${time}`;
  if (diffDays === 1) return `Yesterday ${time}`;

  const opts: Intl.DateTimeFormatOptions =
    date.getFullYear() !== now.getFullYear()
      ? { month: "short", day: "numeric", year: "numeric" }
      : { month: "short", day: "numeric" };

  return `${date.toLocaleDateString([], opts)} ${time}`;
}

export function fmtDurationMs(ms: number | null | undefined): string {
  if (ms == null) return "--";
  if (ms >= 120_000) return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1000)}s`;
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`;
  return `${ms}ms`;
}

export function repoName(repoId: string | null | undefined): string {
  if (!repoId) return "--";
  return repoId.split("/").pop() ?? repoId;
}

export function formatModelName(raw: string | null | undefined): string {
  if (!raw || raw === "unknown" || raw === "<synthetic>") return "Unknown";

  if (raw.includes(",")) {
    return raw
      .split(",")
      .map((item) => formatModelName(item.trim()))
      .join(", ");
  }

  const normalized = raw.toLowerCase().trim();

  const parseSuffixes = (value: string) => {
    let current = value;
    let thinking = false;
    let effort = "";
    let codex = false;
    let preview = false;

    if (current.includes("-thinking")) {
      thinking = true;
      current = current.replace("-thinking", "");
    }

    if (current.includes("-max")) {
      effort = "Max";
      current = current.replace("-max", "");
    } else if (current.includes("-high")) {
      effort = "High";
      current = current.replace("-high", "");
    }

    if (current.includes("-codex")) {
      codex = true;
      current = current.replace("-codex", "");
    }

    if (current.includes("-preview")) {
      preview = true;
      current = current.replace("-preview", "");
    }

    const parts: string[] = [];
    if (codex) parts.push("Codex");
    if (thinking) parts.push("Thinking");
    if (effort) parts.push(`${effort} Effort`);
    if (preview) parts.push("Preview");

    return { base: current, suffix: parts.length > 0 ? ` (${parts.join(", ")})` : "" };
  };

  if (
    normalized.includes("claude") ||
    normalized.includes("opus") ||
    normalized.includes("sonnet") ||
    normalized.includes("haiku")
  ) {
    const { base, suffix } = parseSuffixes(normalized);
    const versionMatch = base.match(/(\d+)[\._-]?(\d+)?/);
    const version = versionMatch
      ? versionMatch[1] + (versionMatch[2] ? `.${versionMatch[2]}` : "")
      : "";
    let family = "";
    if (base.includes("opus")) family = "Opus";
    if (base.includes("sonnet")) family = "Sonnet";
    if (base.includes("haiku")) family = "Haiku";
    return `Claude ${version ? `${version} ` : ""}${family}${suffix}`.trim();
  }

  if (/^gpt[._-]?\d/.test(normalized)) {
    const { base, suffix } = parseSuffixes(normalized);
    const versionMatch = base.match(/(\d+[.\d]*)/);
    const version = versionMatch ? versionMatch[1] : "";
    return `GPT-${version}${suffix}`;
  }

  if (/^o\d/.test(normalized)) return raw;

  if (normalized.startsWith("gemini")) {
    const { base, suffix } = parseSuffixes(normalized);
    const rest = base.replace(/^gemini[._-]?/, "").replace(/-/g, " ").trim();
    const parts = rest.split(" ").map((part) => part.charAt(0).toUpperCase() + part.slice(1));
    return `Gemini ${parts.join(" ")}${suffix}`.trim();
  }

  if (normalized === "default") return "Auto";
  if (normalized.startsWith("composer-")) return `Composer ${raw.slice(9)}`;

  return raw;
}

export function formatPath(path: string | null | undefined): string {
  if (!path) return "--";
  const home = path.match(/^(\/Users\/[^/]+|\/home\/[^/]+)/);
  if (home) {
    return `~${path.slice(home[0].length)}`;
  }
  return path;
}

export function fmtSyncTime(iso: string | null | undefined): string {
  if (!iso) return "Never";
  const date = new Date(iso);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffMinutes = Math.floor(diffMs / 60_000);

  if (diffMinutes < 1) return "Just now";
  if (diffMinutes < 60) return `${diffMinutes}m ago`;

  const diffHours = Math.floor(diffMinutes / 60);
  if (diffHours < 24) return `${diffHours}h ago`;

  return `${date.toLocaleDateString([], { month: "short", day: "numeric" })} ${date.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  })}`;
}
