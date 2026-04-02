import { NavLink, Outlet, useLocation } from "react-router-dom";
import { PeriodProvider, usePeriod } from "@/lib/period";
import type { Period } from "@/lib/types";
import { cn } from "@/lib/utils";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

const PERIODS: Array<{ value: Period; label: string }> = [
  { value: "today", label: "Today" },
  { value: "week", label: "Week" },
  { value: "month", label: "Month" },
  { value: "all", label: "All" },
];

const NAV_ITEMS = [
  { to: "/", label: "Overview", end: true },
  { to: "/insights", label: "Insights" },
  { to: "/sessions", label: "Sessions" },
  { to: "/settings", label: "Settings" },
];

function PeriodSelector({ hidden }: { hidden: boolean }) {
  const { period, setPeriod } = usePeriod();

  if (hidden) return null;

  return (
    <Tabs value={period} onValueChange={(value) => setPeriod(value as Period)}>
      <TabsList>
        {PERIODS.map((item) => (
          <TabsTrigger key={item.value} value={item.value}>
            {item.label}
          </TabsTrigger>
        ))}
      </TabsList>
    </Tabs>
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
              <p className="text-xs text-muted-foreground">Dashboard v2</p>
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
