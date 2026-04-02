import type { Period } from "@/lib/types";

function weekStart(date: Date): Date {
  const day = date.getDay();
  const offset = day === 0 ? 6 : day - 1;
  return new Date(date.getFullYear(), date.getMonth(), date.getDate() - offset);
}

export function periodRange(period: Period): { since?: string; until?: string } {
  const now = new Date();
  const y = now.getFullYear();
  const m = now.getMonth();
  const d = now.getDate();

  const toIso = (value: Date) => value.toISOString();

  switch (period) {
    case "today":
      return { since: toIso(new Date(y, m, d)), until: toIso(new Date(y, m, d + 1)) };
    case "week": {
      const monday = weekStart(now);
      return {
        since: toIso(monday),
        until: toIso(new Date(monday.getFullYear(), monday.getMonth(), monday.getDate() + 7)),
      };
    }
    case "month":
      return { since: toIso(new Date(y, m, 1)), until: toIso(new Date(y, m + 1, 1)) };
    case "all":
      return {};
    default:
      return { since: toIso(new Date(y, m, d)), until: toIso(new Date(y, m, d + 1)) };
  }
}

export function granularityForPeriod(period: Period): "hour" | "day" | "month" {
  switch (period) {
    case "today":
      return "hour";
    case "week":
    case "month":
      return "day";
    case "all":
      return "month";
    default:
      return "day";
  }
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
