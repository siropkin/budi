import { type NextRequest } from "next/server";
import { createAdminClient } from "@/lib/supabase/admin";

// ADR-0083 §7: Max body size 1 MiB
const MAX_BODY_BYTES = 1024 * 1024;

const CURRENT_SCHEMA_VERSION = 1;

interface DailyRollup {
  bucket_day: string;
  role: string;
  provider: string;
  model: string;
  repo_id: string;
  git_branch: string;
  ticket?: string | null;
  message_count: number;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  cost_cents: number;
}

interface SessionSummary {
  session_id: string;
  provider: string;
  started_at?: string | null;
  ended_at?: string | null;
  duration_ms?: number | null;
  repo_id?: string | null;
  git_branch?: string | null;
  ticket?: string | null;
  message_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost_cents: number;
}

interface SyncEnvelope {
  schema_version: number;
  device_id: string;
  org_id: string;
  synced_at: string;
  payload: {
    daily_rollups: DailyRollup[];
    session_summaries: SessionSummary[];
  };
}

/**
 * Authenticate the request via Bearer token.
 * Returns the user row or null if auth fails.
 */
async function authenticateApiKey(
  supabase: ReturnType<typeof createAdminClient>,
  authHeader: string | null
) {
  if (!authHeader?.startsWith("Bearer ")) return null;

  const apiKey = authHeader.slice(7);
  if (!apiKey.startsWith("budi_")) return null;

  const { data, error } = await supabase
    .from("users")
    .select("id, org_id, role")
    .eq("api_key", apiKey)
    .single();

  if (error || !data) return null;
  return data;
}

/**
 * Validate the sync envelope structure.
 * Returns an error message or null if valid.
 */
function validateEnvelope(body: SyncEnvelope): string | null {
  if (body.schema_version !== CURRENT_SCHEMA_VERSION) {
    return `Unsupported schema_version: ${body.schema_version}. Expected ${CURRENT_SCHEMA_VERSION}.`;
  }
  if (!body.device_id || typeof body.device_id !== "string") {
    return "Missing or invalid device_id";
  }
  if (!body.org_id || typeof body.org_id !== "string") {
    return "Missing or invalid org_id";
  }
  if (!body.payload) {
    return "Missing payload";
  }
  if (!Array.isArray(body.payload.daily_rollups)) {
    return "payload.daily_rollups must be an array";
  }
  if (!Array.isArray(body.payload.session_summaries)) {
    return "payload.session_summaries must be an array";
  }
  return null;
}

/**
 * POST /v1/ingest
 *
 * Receives sync payload from daemon per ADR-0083 §7.
 * Auth: Authorization: Bearer budi_<key>
 * Body: sync envelope (ADR-0083 §2)
 * UPSERT semantics per ADR-0083 §5.
 */
export async function POST(request: NextRequest) {
  // --- Body size check ---
  const contentLength = request.headers.get("content-length");
  if (contentLength && parseInt(contentLength, 10) > MAX_BODY_BYTES) {
    return Response.json(
      { error: "Request body exceeds 1 MiB limit" },
      { status: 413 }
    );
  }

  // --- Auth ---
  const supabase = createAdminClient();
  const user = await authenticateApiKey(
    supabase,
    request.headers.get("authorization")
  );
  if (!user) {
    return Response.json({ error: "Unauthorized" }, { status: 401 });
  }

  // --- Parse body ---
  let body: SyncEnvelope;
  try {
    const text = await request.text();
    if (text.length > MAX_BODY_BYTES) {
      return Response.json(
        { error: "Request body exceeds 1 MiB limit" },
        { status: 413 }
      );
    }
    body = JSON.parse(text);
  } catch {
    return Response.json({ error: "Invalid JSON" }, { status: 400 });
  }

  // --- Validate envelope ---
  const validationError = validateEnvelope(body);
  if (validationError) {
    return Response.json({ error: validationError }, { status: 422 });
  }

  // --- Verify org ownership ---
  if (body.org_id !== user.org_id) {
    return Response.json(
      { error: "org_id does not match authenticated user's org" },
      { status: 401 }
    );
  }

  // --- Verify device belongs to user's org ---
  const { data: device, error: deviceError } = await supabase
    .from("devices")
    .select("id, user_id")
    .eq("id", body.device_id)
    .single();

  if (deviceError || !device) {
    // Auto-register the device if it doesn't exist yet
    const { error: insertError } = await supabase.from("devices").insert({
      id: body.device_id,
      user_id: user.id,
      first_seen: new Date().toISOString(),
      last_seen: new Date().toISOString(),
    });
    if (insertError) {
      console.error("Failed to register device:", insertError);
      return Response.json(
        { error: "Failed to register device" },
        { status: 500 }
      );
    }
  } else {
    // Verify device belongs to a user in the same org
    const { data: deviceOwner } = await supabase
      .from("users")
      .select("org_id")
      .eq("id", device.user_id)
      .single();

    if (!deviceOwner || deviceOwner.org_id !== user.org_id) {
      return Response.json(
        { error: "Device does not belong to your org" },
        { status: 401 }
      );
    }

    // Update last_seen
    await supabase
      .from("devices")
      .update({ last_seen: new Date().toISOString() })
      .eq("id", body.device_id);
  }

  // --- UPSERT daily rollups (ADR-0083 §5) ---
  let recordsUpserted = 0;

  if (body.payload.daily_rollups.length > 0) {
    const rollupRows = body.payload.daily_rollups.map((r) => ({
      device_id: body.device_id,
      bucket_day: r.bucket_day,
      role: r.role,
      provider: r.provider,
      model: r.model,
      repo_id: r.repo_id,
      git_branch: r.git_branch,
      ticket: r.ticket ?? null,
      message_count: r.message_count,
      input_tokens: r.input_tokens,
      output_tokens: r.output_tokens,
      cache_creation_tokens: r.cache_creation_tokens,
      cache_read_tokens: r.cache_read_tokens,
      cost_cents: r.cost_cents,
      synced_at: body.synced_at,
    }));

    const { error: rollupError, count } = await supabase
      .from("daily_rollups")
      .upsert(rollupRows, {
        onConflict:
          "device_id,bucket_day,role,provider,model,repo_id,git_branch",
        count: "exact",
      });

    if (rollupError) {
      console.error("Failed to upsert daily_rollups:", rollupError);
      return Response.json(
        { error: "Failed to store daily rollups" },
        { status: 500 }
      );
    }
    recordsUpserted += count ?? rollupRows.length;
  }

  // --- UPSERT session summaries (ADR-0083 §5) ---
  if (body.payload.session_summaries.length > 0) {
    const sessionRows = body.payload.session_summaries.map((s) => ({
      device_id: body.device_id,
      session_id: s.session_id,
      provider: s.provider,
      started_at: s.started_at ?? null,
      ended_at: s.ended_at ?? null,
      duration_ms: s.duration_ms ?? null,
      repo_id: s.repo_id ?? null,
      git_branch: s.git_branch ?? null,
      ticket: s.ticket ?? null,
      message_count: s.message_count,
      total_input_tokens: s.total_input_tokens,
      total_output_tokens: s.total_output_tokens,
      total_cost_cents: s.total_cost_cents,
      synced_at: body.synced_at,
    }));

    const { error: sessionError, count } = await supabase
      .from("session_summaries")
      .upsert(sessionRows, {
        onConflict: "device_id,session_id",
        count: "exact",
      });

    if (sessionError) {
      console.error("Failed to upsert session_summaries:", sessionError);
      return Response.json(
        { error: "Failed to store session summaries" },
        { status: 500 }
      );
    }
    recordsUpserted += count ?? sessionRows.length;
  }

  // --- Compute watermark: latest fully-synced bucket_day ---
  const { data: watermarkRow } = await supabase
    .from("daily_rollups")
    .select("bucket_day")
    .eq("device_id", body.device_id)
    .order("bucket_day", { ascending: false })
    .limit(1)
    .single();

  const watermark = watermarkRow?.bucket_day ?? null;

  // --- ADR-0083 §5: Success response ---
  return Response.json({
    accepted: true,
    watermark,
    records_upserted: recordsUpserted,
  });
}
