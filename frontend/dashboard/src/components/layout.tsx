import { NavLink, Outlet, useLocation } from "react-router-dom";
import { PeriodProvider, usePeriod } from "@/lib/period";
import type { DateRangePreset } from "@/lib/types";
import { cn } from "@/lib/utils";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

const QUICK_PRESETS: Array<{ value: DateRangePreset; label: string }> = [
  { value: "today", label: "Today" },
  { value: "last_7_days", label: "Last 7 days" },
  { value: "last_30_days", label: "Last 30 days" },
  { value: "all", label: "All" },
];

const NAV_ITEMS = [
  { to: "/", label: "Overview", end: true },
  { to: "/insights", label: "Insights" },
  { to: "/sessions", label: "Sessions" },
  { to: "/settings", label: "Settings" },
];

function PeriodSelector({ hidden }: { hidden: boolean }) {
  const { period, setPreset } = usePeriod();

  if (hidden) return null;

  return (
    <Select
      value={period.preset}
      onValueChange={(value) => {
        const preset = value as DateRangePreset;
        setPreset(preset);
      }}
    >
      <SelectTrigger aria-label="Date range preset" className="h-9 w-[170px]">
        <SelectValue placeholder="Select range" />
      </SelectTrigger>
      <SelectContent align="end">
        {QUICK_PRESETS.map((item) => (
          <SelectItem key={item.value} value={item.value}>
            {item.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function ShellBody() {
  const location = useLocation();
  const periodHidden = location.pathname.startsWith("/settings");
  const inSessionDetail = /^\/sessions\/.+/.test(location.pathname);

  return (
    <div className="mx-auto min-h-screen w-full max-w-7xl px-4 pb-10 pt-6 md:px-6">
      <header className="mb-6 rounded-lg border bg-card px-4 py-4 md:px-6">
        <div className="flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
          <div className="flex flex-col gap-3 lg:flex-row lg:items-center lg:gap-5">
            <div className="flex items-center gap-4">
              <div className="flex h-10 w-10 items-center justify-center rounded-md border bg-muted text-lg">📊</div>
              <div className="space-y-1">
                <h1 className="text-xl font-semibold tracking-tight text-foreground">budi</h1>
              </div>
            </div>
            <nav className="flex flex-wrap items-center gap-2">
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
                  {item.to === "/sessions" && inSessionDetail ? "↑ Sessions" : item.label}
                </NavLink>
              ))}
            </nav>
          </div>
          <div className="lg:ml-4">
            <PeriodSelector hidden={periodHidden} />
          </div>
        </div>
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
