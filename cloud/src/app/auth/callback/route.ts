import { NextResponse } from "next/server";
import { createClient } from "@/lib/supabase/server";
import { createAdminClient } from "@/lib/supabase/admin";
import { randomBytes } from "crypto";

/**
 * OAuth callback handler for Supabase Auth.
 * Exchanges the code for a session, then ensures a budi `users` row exists.
 */
export async function GET(request: Request) {
  const { searchParams, origin } = new URL(request.url);
  const code = searchParams.get("code");
  const next = searchParams.get("next") ?? "/dashboard";

  if (!code) {
    return NextResponse.redirect(`${origin}/login?error=missing_code`);
  }

  const supabase = await createClient();
  const { error } = await supabase.auth.exchangeCodeForSession(code);

  if (error) {
    return NextResponse.redirect(`${origin}/login?error=auth_failed`);
  }

  // Get the authenticated user
  const {
    data: { user },
  } = await supabase.auth.getUser();

  if (!user) {
    return NextResponse.redirect(`${origin}/login?error=no_user`);
  }

  // Ensure a budi users row exists (uses admin client to bypass RLS for upsert)
  const admin = createAdminClient();
  const { data: existingUser } = await admin
    .from("users")
    .select("id, org_id, display_name")
    .eq("id", user.id)
    .single();

  if (!existingUser) {
    // First sign-in: create the users row
    const apiKey = `budi_${randomBytes(24).toString("base64url")}`;
    const displayName =
      user.user_metadata?.full_name ||
      user.user_metadata?.name ||
      user.email?.split("@")[0] ||
      "User";

    await admin.from("users").insert({
      id: user.id,
      org_id: null, // Will be set when creating/joining an org
      role: "manager", // First user defaults to manager
      api_key: apiKey,
      display_name: displayName,
      email: user.email,
    });

    // New user with no org — redirect to setup
    return NextResponse.redirect(`${origin}/dashboard/setup`);
  }

  // Existing user — update email/display_name if changed
  const displayName =
    user.user_metadata?.full_name ||
    user.user_metadata?.name ||
    existingUser.display_name;
  if (displayName || user.email) {
    await admin
      .from("users")
      .update({
        ...(displayName && { display_name: displayName }),
        ...(user.email && { email: user.email }),
      })
      .eq("id", user.id);
  }

  // If user has no org, redirect to setup
  if (!existingUser.org_id) {
    return NextResponse.redirect(`${origin}/dashboard/setup`);
  }

  return NextResponse.redirect(`${origin}${next}`);
}
