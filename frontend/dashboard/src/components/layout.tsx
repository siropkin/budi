import { useEffect, useState } from "react";
import { NavLink, Outlet, useLocation } from "react-router-dom";
import { PeriodProvider, usePeriod } from "@/lib/period";
import type { DateRangePreset } from "@/lib/types";
import { periodLabel } from "@/lib/format";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

const QUICK_PRESETS: Array<{ value: DateRangePreset; label: string }> = [
  { value: "today", label: "Today" },
  { value: "month_to_date", label: "Month to date" },
  { value: "last_7_days", label: "Last 7 days" },
  { value: "last_30_days", label: "Last 30 days" },
  { value: "last_month", label: "Last Month" },
  { value: "custom", label: "Custom range" },
];

const NAV_ITEMS = [
  { to: "/", label: "Overview", end: true },
  { to: "/insights", label: "Insights" },
  { to: "/sessions", label: "Sessions" },
  { to: "/settings", label: "Settings" },
];

function PeriodSelector({ hidden }: { hidden: boolean }) {
  const { period, setPreset, setCustomRange } = usePeriod();
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");

  useEffect(() => {
    if (period.preset !== "custom") return;
    setFrom(period.from ?? "");
    setTo(period.to ?? "");
  }, [period.from, period.preset, period.to]);

  if (hidden) return null;

  const showCustomRange = period.preset === "custom";
  const customRangeInvalid = showCustomRange && (!from || !to || from > to);

  return (
    <div className="flex flex-col gap-2 md:items-end">
      <div className="flex flex-wrap items-center gap-2">
        <select
          aria-label="Date range preset"
          value={period.preset}
          onChange={(event) => setPreset(event.target.value as DateRangePreset)}
          className="h-9 rounded-md border border-input bg-background px-3 text-sm text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
        >
          {QUICK_PRESETS.map((item) => (
            <option key={item.value} value={item.value}>
              {item.label}
            </option>
          ))}
        </select>
        {showCustomRange ? (
          <>
            <Input
              type="date"
              aria-label="Start date"
              value={from}
              onChange={(event) => setFrom(event.target.value)}
              className="h-9 w-40"
            />
            <span className="text-xs text-muted-foreground">to</span>
            <Input
              type="date"
              aria-label="End date"
              value={to}
              onChange={(event) => setTo(event.target.value)}
              className="h-9 w-40"
            />
            <Button type="button" size="sm" variant="secondary" disabled={customRangeInvalid} onClick={() => setCustomRange(from, to)}>
              Apply
            </Button>
          </>
        ) : null}
      </div>
      <p className="text-xs text-muted-foreground">{periodLabel(period)}</p>
    </div>
  );
}

function ShellBody() {
  const location = useLocation();
  const periodHidden = location.pathname.startsWith("/settings");

  return (
    <div className="mx-auto min-h-screen w-full max-w-7xl px-4 pb-10 pt-6 md:px-6">
      <header className="mb-6 rounded-lg border bg-card px-4 py-4 md:px-6">
        <div className="flex flex-col gap-4 md:flex-row md:items-center md:justify-between">
          <div className="flex items-center gap-4">
            <div className="flex h-10 w-10 items-center justify-center rounded-md border bg-muted text-lg">📊</div>
            <div className="space-y-1">
              <h1 className="text-xl font-semibold tracking-tight text-foreground">budi</h1>
              <p className="text-xs text-muted-foreground">Dashboard</p>
            </div>
          </div>
          <PeriodSelector hidden={periodHidden} />
        </div>
        <nav className="mt-4 flex flex-wrap gap-2">
          {NAV_ITEMS.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              end={item.end}
              className={({ isActive }) =>
                cn(
                  "rounded-md px-3 py-2 text-sm font-medium transition",
                  isActive ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-foreground",
                )
              }
            >
              {item.label}
            </NavLink>
          ))}
        </nav>
      </header>

      <main>
        <Outlet />
      </main>
    </div>
  );
}

export function AppLayout() {
  return (
    <PeriodProvider>
      <ShellBody />
    </PeriodProvider>
  );
}
