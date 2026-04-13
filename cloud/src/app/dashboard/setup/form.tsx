"use client";

import { useActionState } from "react";
import { createOrg } from "@/app/actions/org";

export function OrgSetupForm() {
  const [state, action, pending] = useActionState(createOrg, undefined);

  return (
    <form action={action} className="space-y-4">
      <div>
        <label
          htmlFor="name"
          className="block text-sm font-medium text-zinc-300"
        >
          Organization name
        </label>
        <input
          id="name"
          name="name"
          type="text"
          required
          placeholder="e.g. Acme Engineering"
          className="mt-1 w-full rounded-lg border border-white/10 bg-white/5 px-4 py-2.5 text-sm text-white placeholder:text-zinc-500 focus:border-blue-500 focus:outline-none"
        />
      </div>
      {state?.error && (
        <p className="text-sm text-red-400">{state.error}</p>
      )}
      <button
        type="submit"
        disabled={pending}
        className="w-full rounded-lg bg-blue-600 px-4 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-700 disabled:opacity-50"
      >
        {pending ? "Creating..." : "Create organization"}
      </button>
    </form>
  );
}
