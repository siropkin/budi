import { NavLink, Outlet, useLocation } from "react-router-dom";
import type { DateRange } from "react-day-picker";
import { CalendarIcon } from "lucide-react";
import { PeriodProvider, usePeriod } from "@/lib/period";
import type { DateRangePreset } from "@/lib/types";
import { periodLabel } from "@/lib/format";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

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

  if (hidden) return null;

  const showCustomRange = period.preset === "custom";
  const selectedRange: DateRange | undefined =
    showCustomRange && period.from && period.to
      ? {
          from: new Date(`${period.from}T00:00:00`),
          to: new Date(`${period.to}T00:00:00`),
        }
      : undefined;

  const toDateInput = (date: Date) => {
    const year = String(date.getFullYear());
    const month = String(date.getMonth() + 1).padStart(2, "0");
    const day = String(date.getDate()).padStart(2, "0");
    return `${year}-${month}-${day}`;
  };

  return (
    <div className="flex flex-col gap-2 md:items-end">
      <div className="flex flex-wrap items-center gap-2">
        <Select
          value={period.preset}
          onValueChange={(value) => setPreset(value as DateRangePreset)}
        >
          <SelectTrigger aria-label="Date range preset" className="h-9 w-[190px]">
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
        {showCustomRange ? (
          <Popover>
            <PopoverTrigger asChild>
              <Button type="button" variant="outline" size="sm" className="h-9 min-w-[220px] justify-start text-left font-normal">
                <CalendarIcon className="mr-2 h-4 w-4" />
                {period.from && period.to ? periodLabel(period) : "Pick date range"}
              </Button>
            </PopoverTrigger>
            <PopoverContent align="end" className="w-auto p-0">
              <Calendar
                mode="range"
                numberOfMonths={2}
                selected={selectedRange}
                defaultMonth={selectedRange?.from}
                onSelect={(range) => {
                  if (!range?.from || !range.to) return;
                  setCustomRange(toDateInput(range.from), toDateInput(range.to));
                }}
              />
            </PopoverContent>
          </Popover>
        ) : null}
      </div>
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
