"use client";

import { useRouter, useSearchParams } from "next/navigation";
import { clsx } from "clsx";

const PERIODS = [
  { label: "7d", value: "7" },
  { label: "30d", value: "30" },
  { label: "90d", value: "90" },
] as const;

export function PeriodSelector() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const current = searchParams.get("days") ?? "30";

  function selectPeriod(days: string) {
    const params = new URLSearchParams(searchParams.toString());
    params.set("days", days);
    router.push(`?${params.toString()}`);
  }

  return (
    <div className="flex gap-1 rounded-lg border border-white/10 bg-white/[0.02] p-1">
      {PERIODS.map((p) => (
        <button
          key={p.value}
          onClick={() => selectPeriod(p.value)}
          className={clsx(
            "rounded-md px-3 py-1.5 text-sm font-medium transition-colors",
            current === p.value
              ? "bg-white/10 text-white"
              : "text-zinc-400 hover:text-zinc-200"
          )}
        >
          {p.label}
        </button>
      ))}
    </div>
  );
}
