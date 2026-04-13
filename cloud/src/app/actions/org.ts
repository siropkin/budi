"use server";

import { redirect } from "next/navigation";
import { randomBytes } from "crypto";
import { createClient } from "@/lib/supabase/server";
import { createAdminClient } from "@/lib/supabase/admin";

export async function createOrg(
  _prevState: { error: string } | undefined,
  formData: FormData
) {
  const name = formData.get("name") as string;
  if (!name?.trim()) return { error: "Org name is required" };

  const supabase = await createClient();
  const {
    data: { user },
  } = await supabase.auth.getUser();
  if (!user) return { error: "Not authenticated" };

  const admin = createAdminClient();
  const orgId = `org_${randomBytes(12).toString("base64url")}`;

  // Create the org
  const { error: orgError } = await admin.from("orgs").insert({
    id: orgId,
    name: name.trim(),
  });
  if (orgError) return { error: "Failed to create org" };

  // Link user to org as manager
  const { error: userError } = await admin
    .from("users")
    .update({ org_id: orgId, role: "manager" })
    .eq("id", user.id);
  if (userError) return { error: "Failed to link user to org" };

  redirect("/dashboard");
}

export async function generateInviteToken() {
  const supabase = await createClient();
  const {
    data: { user },
  } = await supabase.auth.getUser();
  if (!user) return { error: "Not authenticated" };

  const admin = createAdminClient();
  const { data: budiUser } = await admin
    .from("users")
    .select("id, org_id, role")
    .eq("id", user.id)
    .single();

  if (!budiUser?.org_id || budiUser.role !== "manager") {
    return { error: "Only managers can create invite tokens" };
  }

  const token = randomBytes(24).toString("base64url");
  const expiresAt = new Date(Date.now() + 7 * 24 * 60 * 60 * 1000); // 7 days

  const { error } = await admin.from("invite_tokens").insert({
    id: token,
    org_id: budiUser.org_id,
    role: "member",
    created_by: budiUser.id,
    expires_at: expiresAt.toISOString(),
  });

  if (error) return { error: "Failed to create invite token" };

  return { token };
}
