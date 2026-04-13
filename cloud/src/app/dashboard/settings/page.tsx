import { getCurrentUser, getOrgMembers } from "@/lib/dal";
import { createAdminClient } from "@/lib/supabase/admin";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { InviteSection } from "./invite-section";

export default async function SettingsPage() {
  const user = await getCurrentUser();
  if (!user?.org_id) return null;

  const admin = createAdminClient();
  const { data: org } = await admin
    .from("orgs")
    .select("id, name")
    .eq("id", user.org_id)
    .single();

  const members = await getOrgMembers(user.org_id);

  return (
    <div className="space-y-6">
      <h1 className="text-xl font-bold">Settings</h1>

      <Card>
        <CardHeader>
          <CardTitle>Organization</CardTitle>
        </CardHeader>
        <CardContent>
          <dl className="space-y-3 text-sm">
            <div className="flex justify-between">
              <dt className="text-zinc-400">Name</dt>
              <dd className="text-zinc-200">{org?.name ?? "-"}</dd>
            </div>
            <div className="flex justify-between">
              <dt className="text-zinc-400">Org ID</dt>
              <dd className="font-mono text-xs text-zinc-400">
                {user.org_id}
              </dd>
            </div>
          </dl>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Your API Key</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="mb-2 text-sm text-zinc-400">
            Use this key with <code className="text-zinc-300">budi cloud join</code> on
            your local machine to start syncing data.
          </p>
          <code className="block rounded-lg bg-black/50 px-4 py-3 font-mono text-sm text-emerald-400">
            {user.api_key}
          </code>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Team Members ({members.length})</CardTitle>
        </CardHeader>
        <CardContent>
          {members.length === 0 ? (
            <p className="text-sm text-zinc-500">No members yet</p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-white/10 text-left text-zinc-400">
                  <th className="pb-2 font-medium">Name</th>
                  <th className="pb-2 font-medium">Email</th>
                  <th className="pb-2 font-medium">Role</th>
                </tr>
              </thead>
              <tbody>
                {members.map((m) => (
                  <tr key={m.id} className="border-b border-white/5">
                    <td className="py-2 text-zinc-200">
                      {m.display_name || "-"}
                    </td>
                    <td className="py-2 text-zinc-400">{m.email || "-"}</td>
                    <td className="py-2">
                      <span
                        className={
                          m.role === "manager"
                            ? "text-blue-400"
                            : "text-zinc-400"
                        }
                      >
                        {m.role}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </CardContent>
      </Card>

      {user.role === "manager" && <InviteSection />}
    </div>
  );
}
