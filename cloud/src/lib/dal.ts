import "server-only";
import { createClient } from "@/lib/supabase/server";
import { createAdminClient } from "@/lib/supabase/admin";

export interface DateRange {
  from: string; // YYYY-MM-DD
  to: string; // YYYY-MM-DD
}

export interface BudiUser {
  id: string;
  org_id: string | null;
  role: string;
  api_key: string;
  display_name: string | null;
  email: string | null;
}

/**
 * Get the current budi user and verify they have an org.
 * Uses admin client because the auth→users mapping needs to bypass RLS
 * during the initial lookup.
 */
export async function getCurrentUser(): Promise<BudiUser | null> {
  const supabase = await createClient();
  const {
    data: { user: authUser },
  } = await supabase.auth.getUser();
  if (!authUser) return null;

  const admin = createAdminClient();
  const { data } = await admin
    .from("users")
    .select("id, org_id, role, api_key, display_name, email")
    .eq("id", authUser.id)
    .single();

  return data;
}

/**
 * Get overview stats for the org.
 */
export async function getOverviewStats(orgId: string, range: DateRange) {
  const admin = createAdminClient();

  // Get all device IDs in this org
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) {
    return {
      totalCostCents: 0,
      totalInputTokens: 0,
      totalOutputTokens: 0,
      totalMessages: 0,
      totalSessions: 0,
    };
  }

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select(
      "cost_cents, input_tokens, output_tokens, message_count, cache_creation_tokens, cache_read_tokens"
    )
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to);

  const { count: sessionCount } = await admin
    .from("session_summaries")
    .select("*", { count: "exact", head: true })
    .in("device_id", deviceIds)
    .gte("started_at", range.from)
    .lte("started_at", range.to + "T23:59:59Z");

  const totals = (rollups ?? []).reduce(
    (acc, r) => ({
      totalCostCents: acc.totalCostCents + Number(r.cost_cents),
      totalInputTokens: acc.totalInputTokens + Number(r.input_tokens),
      totalOutputTokens: acc.totalOutputTokens + Number(r.output_tokens),
      totalMessages: acc.totalMessages + r.message_count,
    }),
    { totalCostCents: 0, totalInputTokens: 0, totalOutputTokens: 0, totalMessages: 0 }
  );

  return { ...totals, totalSessions: sessionCount ?? 0 };
}

/**
 * Get daily cost activity for charts.
 */
export async function getDailyActivity(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("bucket_day, input_tokens, output_tokens, cost_cents, message_count")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to)
    .order("bucket_day");

  // Aggregate by day
  const byDay = new Map<
    string,
    { input_tokens: number; output_tokens: number; cost_cents: number; message_count: number }
  >();
  for (const r of rollups ?? []) {
    const existing = byDay.get(r.bucket_day) ?? {
      input_tokens: 0,
      output_tokens: 0,
      cost_cents: 0,
      message_count: 0,
    };
    existing.input_tokens += Number(r.input_tokens);
    existing.output_tokens += Number(r.output_tokens);
    existing.cost_cents += Number(r.cost_cents);
    existing.message_count += r.message_count;
    byDay.set(r.bucket_day, existing);
  }

  return Array.from(byDay.entries())
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([day, data]) => ({ bucket_day: day, ...data }));
}

/**
 * Get cost breakdown by user/device.
 */
export async function getCostByUser(orgId: string, range: DateRange) {
  const admin = createAdminClient();

  // Get users and their devices
  const { data: orgUsers } = await admin
    .from("users")
    .select("id, display_name, email")
    .eq("org_id", orgId);

  if (!orgUsers?.length) return [];

  const { data: devices } = await admin
    .from("devices")
    .select("id, user_id, label")
    .in(
      "user_id",
      orgUsers.map((u) => u.id)
    );

  const deviceIds = (devices ?? []).map((d) => d.id);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("device_id, cost_cents, input_tokens, output_tokens, message_count")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to);

  // Aggregate by device → user
  const byDevice = new Map<string, number>();
  for (const r of rollups ?? []) {
    byDevice.set(r.device_id, (byDevice.get(r.device_id) ?? 0) + Number(r.cost_cents));
  }

  // Map device costs to users
  const byUser = new Map<string, { name: string; cost_cents: number }>();
  for (const device of devices ?? []) {
    const user = orgUsers.find((u) => u.id === device.user_id);
    const name = user?.display_name || user?.email || device.user_id.slice(0, 8);
    const deviceCost = byDevice.get(device.id) ?? 0;
    const existing = byUser.get(device.user_id);
    if (existing) {
      existing.cost_cents += deviceCost;
    } else {
      byUser.set(device.user_id, { name, cost_cents: deviceCost });
    }
  }

  return Array.from(byUser.values())
    .filter((u) => u.cost_cents > 0)
    .sort((a, b) => b.cost_cents - a.cost_cents);
}

/**
 * Get cost breakdown by model.
 */
export async function getCostByModel(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("provider, model, cost_cents")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to);

  const byModel = new Map<string, { provider: string; model: string; cost_cents: number }>();
  for (const r of rollups ?? []) {
    const key = `${r.provider}:${r.model}`;
    const existing = byModel.get(key);
    if (existing) {
      existing.cost_cents += Number(r.cost_cents);
    } else {
      byModel.set(key, {
        provider: r.provider,
        model: r.model,
        cost_cents: Number(r.cost_cents),
      });
    }
  }

  return Array.from(byModel.values())
    .filter((m) => m.cost_cents > 0)
    .sort((a, b) => b.cost_cents - a.cost_cents);
}

/**
 * Get cost breakdown by repo.
 */
export async function getCostByRepo(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("repo_id, cost_cents")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to);

  const byRepo = new Map<string, number>();
  for (const r of rollups ?? []) {
    byRepo.set(r.repo_id, (byRepo.get(r.repo_id) ?? 0) + Number(r.cost_cents));
  }

  return Array.from(byRepo.entries())
    .map(([repo_id, cost_cents]) => ({ repo_id, cost_cents }))
    .filter((r) => r.cost_cents > 0)
    .sort((a, b) => b.cost_cents - a.cost_cents);
}

/**
 * Get cost breakdown by branch.
 */
export async function getCostByBranch(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("repo_id, git_branch, cost_cents")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to);

  const byBranch = new Map<string, { repo_id: string; git_branch: string; cost_cents: number }>();
  for (const r of rollups ?? []) {
    const key = `${r.repo_id}:${r.git_branch}`;
    const existing = byBranch.get(key);
    if (existing) {
      existing.cost_cents += Number(r.cost_cents);
    } else {
      byBranch.set(key, {
        repo_id: r.repo_id,
        git_branch: r.git_branch,
        cost_cents: Number(r.cost_cents),
      });
    }
  }

  return Array.from(byBranch.values())
    .filter((b) => b.cost_cents > 0)
    .sort((a, b) => b.cost_cents - a.cost_cents);
}

/**
 * Get cost breakdown by ticket.
 */
export async function getCostByTicket(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: rollups } = await admin
    .from("daily_rollups")
    .select("ticket, cost_cents")
    .in("device_id", deviceIds)
    .gte("bucket_day", range.from)
    .lte("bucket_day", range.to)
    .not("ticket", "is", null);

  const byTicket = new Map<string, number>();
  for (const r of rollups ?? []) {
    if (r.ticket) {
      byTicket.set(r.ticket, (byTicket.get(r.ticket) ?? 0) + Number(r.cost_cents));
    }
  }

  return Array.from(byTicket.entries())
    .map(([ticket, cost_cents]) => ({ ticket, cost_cents }))
    .filter((t) => t.cost_cents > 0)
    .sort((a, b) => b.cost_cents - a.cost_cents);
}

/**
 * Get sessions list.
 */
export async function getSessions(orgId: string, range: DateRange) {
  const admin = createAdminClient();
  const deviceIds = await getOrgDeviceIds(admin, orgId);
  if (deviceIds.length === 0) return [];

  const { data: sessions } = await admin
    .from("session_summaries")
    .select("*")
    .in("device_id", deviceIds)
    .gte("started_at", range.from)
    .lte("started_at", range.to + "T23:59:59Z")
    .order("started_at", { ascending: false })
    .limit(100);

  return sessions ?? [];
}

/**
 * Get org members list.
 */
export async function getOrgMembers(orgId: string) {
  const admin = createAdminClient();
  const { data } = await admin
    .from("users")
    .select("id, display_name, email, role, created_at")
    .eq("org_id", orgId)
    .order("created_at");

  return data ?? [];
}

// --- Helpers ---

async function getOrgDeviceIds(
  admin: ReturnType<typeof createAdminClient>,
  orgId: string
): Promise<string[]> {
  const { data: users } = await admin
    .from("users")
    .select("id")
    .eq("org_id", orgId);

  if (!users?.length) return [];

  const { data: devices } = await admin
    .from("devices")
    .select("id")
    .in(
      "user_id",
      users.map((u) => u.id)
    );

  return (devices ?? []).map((d) => d.id);
}
