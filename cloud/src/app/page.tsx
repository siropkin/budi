import { redirect } from "next/navigation";
import { createClient } from "@/lib/supabase/server";
import Link from "next/link";

export const dynamic = "force-dynamic";

export default async function Home() {
  const supabase = await createClient();
  const {
    data: { user },
  } = await supabase.auth.getUser();

  if (user) redirect("/dashboard");

  return (
    <main className="flex flex-1 items-center justify-center bg-[#0a0a0a]">
      <div className="text-center">
        <h1 className="text-3xl font-bold text-white">Budi Cloud</h1>
        <p className="mt-2 text-zinc-400">
          Team-wide AI cost visibility for engineering managers.
        </p>
        <Link
          href="/login"
          className="mt-6 inline-block rounded-lg bg-blue-600 px-6 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-700"
        >
          Sign in
        </Link>
      </div>
    </main>
  );
}
