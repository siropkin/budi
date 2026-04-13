import { redirect } from "next/navigation";
import { createClient } from "@/lib/supabase/server";
import { createAdminClient } from "@/lib/supabase/admin";
import { randomBytes } from "crypto";

export const dynamic = "force-dynamic";

export default async function InvitePage({
  params,
}: {
  params: Promise<{ token: string }>;
}) {
  const { token } = await params;
  const admin = createAdminClient();

  // Validate the invite token
  const { data: invite } = await admin
    .from("invite_tokens")
    .select("id, org_id, role, expires_at, used_by")
    .eq("id", token)
    .single();

  if (!invite) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-[#0a0a0a]">
        <div className="text-center">
          <h1 className="text-xl font-bold text-white">Invalid Invite</h1>
          <p className="mt-2 text-zinc-400">
            This invite link is invalid or has already been used.
          </p>
        </div>
      </main>
    );
  }

  if (invite.used_by) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-[#0a0a0a]">
        <div className="text-center">
          <h1 className="text-xl font-bold text-white">Already Used</h1>
          <p className="mt-2 text-zinc-400">
            This invite link has already been used.
          </p>
        </div>
      </main>
    );
  }

  if (new Date(invite.expires_at) < new Date()) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-[#0a0a0a]">
        <div className="text-center">
          <h1 className="text-xl font-bold text-white">Expired</h1>
          <p className="mt-2 text-zinc-400">
            This invite link has expired. Ask your manager for a new one.
          </p>
        </div>
      </main>
    );
  }

  // Check if user is authenticated
  const supabase = await createClient();
  const {
    data: { user: authUser },
  } = await supabase.auth.getUser();

  if (!authUser) {
    // Redirect to login with return URL
    redirect(`/login?next=/invite/${token}`);
  }

  // Check if user already exists
  const { data: existingUser } = await admin
    .from("users")
    .select("id, org_id")
    .eq("id", authUser.id)
    .single();

  if (existingUser?.org_id) {
    // User already in an org
    if (existingUser.org_id === invite.org_id) {
      redirect("/dashboard");
    }
    return (
      <main className="flex min-h-screen items-center justify-center bg-[#0a0a0a]">
        <div className="text-center">
          <h1 className="text-xl font-bold text-white">
            Already in an Organization
          </h1>
          <p className="mt-2 text-zinc-400">
            You are already a member of another organization. Multi-org is not
            supported yet.
          </p>
        </div>
      </main>
    );
  }

  // Join the org
  if (existingUser) {
    // User exists but has no org
    await admin
      .from("users")
      .update({ org_id: invite.org_id, role: invite.role })
      .eq("id", authUser.id);
  } else {
    // New user
    const apiKey = `budi_${randomBytes(24).toString("base64url")}`;
    const displayName =
      authUser.user_metadata?.full_name ||
      authUser.user_metadata?.name ||
      authUser.email?.split("@")[0] ||
      "User";

    await admin.from("users").insert({
      id: authUser.id,
      org_id: invite.org_id,
      role: invite.role,
      api_key: apiKey,
      display_name: displayName,
      email: authUser.email,
    });
  }

  // Mark token as used
  await admin
    .from("invite_tokens")
    .update({ used_by: authUser.id, used_at: new Date().toISOString() })
    .eq("id", token);

  redirect("/dashboard");
}
