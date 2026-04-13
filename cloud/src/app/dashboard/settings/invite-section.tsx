"use client";

import { useState } from "react";
import { generateInviteToken } from "@/app/actions/org";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export function InviteSection() {
  const [inviteUrl, setInviteUrl] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleGenerate() {
    setLoading(true);
    setError(null);

    const result = await generateInviteToken();
    setLoading(false);

    if (result.error) {
      setError(result.error);
    } else if (result.token) {
      setInviteUrl(`${window.location.origin}/invite/${result.token}`);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Invite Team Members</CardTitle>
      </CardHeader>
      <CardContent>
        <p className="mb-3 text-sm text-zinc-400">
          Generate an invite link to share with your team. Links expire in 7
          days.
        </p>

        {error && <p className="mb-3 text-sm text-red-400">{error}</p>}

        {inviteUrl ? (
          <div className="space-y-3">
            <code className="block rounded-lg bg-black/50 px-4 py-3 font-mono text-xs text-emerald-400 break-all">
              {inviteUrl}
            </code>
            <button
              onClick={() => {
                navigator.clipboard.writeText(inviteUrl);
              }}
              className="rounded-lg bg-white/10 px-4 py-2 text-sm font-medium text-zinc-200 transition-colors hover:bg-white/15"
            >
              Copy link
            </button>
          </div>
        ) : (
          <button
            onClick={handleGenerate}
            disabled={loading}
            className="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-blue-700 disabled:opacity-50"
          >
            {loading ? "Generating..." : "Generate invite link"}
          </button>
        )}
      </CardContent>
    </Card>
  );
}
