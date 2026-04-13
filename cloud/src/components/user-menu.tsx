import { signOut } from "@/app/actions/auth";
import { LogOut } from "lucide-react";

export function UserMenu({
  displayName,
  email,
}: {
  displayName: string | null;
  email: string | null;
}) {
  return (
    <div className="flex items-center gap-3">
      <div className="text-right">
        <p className="text-sm font-medium text-zinc-200">
          {displayName || "User"}
        </p>
        {email && <p className="text-xs text-zinc-500">{email}</p>}
      </div>
      <form action={signOut}>
        <button
          type="submit"
          className="rounded-lg p-2 text-zinc-400 transition-colors hover:bg-white/5 hover:text-zinc-200"
          title="Sign out"
        >
          <LogOut className="h-4 w-4" />
        </button>
      </form>
    </div>
  );
}
