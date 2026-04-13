/**
 * Formatting utilities — reuses patterns from frontend/dashboard/src/lib/format.ts.
 */

/** Format a dollar cost from cents. */
export function fmtCost(cents: number): string {
  const dollars = cents / 100;
  if (dollars === 0) return "$0.00";
  if (Math.abs(dollars) < 0.01) return `$${dollars.toFixed(4)}`;
  if (Math.abs(dollars) < 1) return `$${dollars.toFixed(2)}`;
  return `$${dollars.toFixed(2)}`;
}

/** Format a number with locale-aware grouping. */
export function fmtNum(n: number): string {
  if (n === 0) return "0";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toLocaleString("en-US");
}

/** Extract a short repo name from a hashed repo_id. */
export function repoName(repoId: string | null): string {
  if (!repoId) return "(unknown)";
  if (repoId.startsWith("sha256:")) return repoId.slice(7, 15) + "...";
  if (repoId.length > 16) return repoId.slice(0, 12) + "...";
  return repoId;
}

/** Format a model name for display. */
export function formatModelName(model: string): string {
  // Strip date suffixes like -20250514
  return model.replace(/-\d{8}$/, "");
}

/** Format a date string for chart labels. */
export function fmtDate(dateStr: string): string {
  const d = new Date(dateStr + "T00:00:00");
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}
