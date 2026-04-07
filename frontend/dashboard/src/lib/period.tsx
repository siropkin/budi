import { createContext, useContext, useMemo, useState } from "react";
import type { DashboardFilters, DateRangePreset, DateRangeSelection } from "@/lib/types";

interface DashboardFiltersState {
  filters: DashboardFilters;
  setPreset: (preset: DateRangePreset) => void;
  setCustomRange: (from: string, to: string) => void;
  setDimension: (key: "agents" | "models" | "projects" | "branches", values: string[]) => void;
  clearDimensions: () => void;
}

interface PeriodState {
  period: DateRangeSelection;
  setPreset: (preset: DateRangePreset) => void;
  setCustomRange: (from: string, to: string) => void;
}

const DashboardFiltersContext = createContext<DashboardFiltersState | null>(null);
const STORAGE_KEY = "budi_dashboard_filters_v1";

const QUICK_PRESETS: DateRangePreset[] = ["today", "last_7_days", "last_30_days", "all", "custom"];

function validPreset(value: unknown): value is DateRangePreset {
  return typeof value === "string" && QUICK_PRESETS.includes(value as DateRangePreset);
}

function validDateOnly(value: unknown): value is string {
  return typeof value === "string" && /^\d{4}-\d{2}-\d{2}$/.test(value);
}

function normalizePeriod(value: unknown): DateRangeSelection | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<DateRangeSelection>;
  if (!validPreset(candidate.preset)) return null;

  if (candidate.preset === "custom") {
    if (!validDateOnly(candidate.from) || !validDateOnly(candidate.to)) {
      return null;
    }
    return { preset: "custom", from: candidate.from, to: candidate.to };
  }

  return { preset: candidate.preset };
}

function normalizeList(values: unknown): string[] {
  if (!Array.isArray(values)) return [];
  const unique = new Set<string>();
  for (const value of values) {
    if (typeof value !== "string") continue;
    const trimmed = value.trim();
    if (!trimmed) continue;
    unique.add(trimmed);
  }
  return Array.from(unique).sort((a, b) => a.localeCompare(b));
}

function normalizeFilters(value: unknown): DashboardFilters | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<DashboardFilters>;
  const period = normalizePeriod(candidate.period);
  if (!period) return null;

  return {
    period,
    agents: normalizeList(candidate.agents),
    models: normalizeList(candidate.models),
    projects: normalizeList(candidate.projects),
    branches: normalizeList(candidate.branches),
  };
}

function legacyPeriodRange(value: string | null): DateRangeSelection | null {
  switch (value) {
    case "today":
      return { preset: "today" };
    case "week":
      return { preset: "last_7_days" };
    case "month":
      return { preset: "last_30_days" };
    case "all":
      return { preset: "all" };
    default:
      return null;
  }
}

function defaultFilters(): DashboardFilters {
  return {
    period: { preset: "today" },
    agents: [],
    models: [],
    projects: [],
    branches: [],
  };
}

function persistFilters(nextFilters: DashboardFilters) {
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(nextFilters));
  window.localStorage.setItem("budi_period_range", JSON.stringify(nextFilters.period));
  window.localStorage.removeItem("budi_period");
}

function todayDateOnly(): string {
  const now = new Date();
  const year = now.getFullYear();
  const month = `${now.getMonth() + 1}`.padStart(2, "0");
  const day = `${now.getDate()}`.padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function getInitialFilters(): DashboardFilters {
  const stored = window.localStorage.getItem(STORAGE_KEY);
  if (stored) {
    try {
      const parsed = normalizeFilters(JSON.parse(stored) as unknown);
      if (parsed) return parsed;
    } catch {
      // Ignore malformed storage and fall back to defaults.
    }
  }

  const legacyRange = window.localStorage.getItem("budi_period_range");
  if (legacyRange) {
    try {
      const parsed = normalizePeriod(JSON.parse(legacyRange) as unknown);
      if (parsed) {
        return { ...defaultFilters(), period: parsed };
      }
    } catch {
      // Ignore malformed storage and fall back to defaults.
    }
  }

  const legacy = legacyPeriodRange(window.localStorage.getItem("budi_period"));
  if (legacy) {
    return { ...defaultFilters(), period: legacy };
  }

  return defaultFilters();
}

export function DashboardFiltersProvider({ children }: { children: React.ReactNode }) {
  const [filters, setFilters] = useState<DashboardFilters>(getInitialFilters);

  const setPreset = (preset: DateRangePreset) => {
    setFilters((current) => {
      const nextPeriod =
        preset === "custom"
          ? {
              preset,
              from: current.period.preset === "custom" ? current.period.from ?? todayDateOnly() : todayDateOnly(),
              to: current.period.preset === "custom" ? current.period.to ?? todayDateOnly() : todayDateOnly(),
            }
          : { preset };
      const nextFilters = { ...current, period: nextPeriod };
      persistFilters(nextFilters);
      return nextFilters;
    });
  };

  const setCustomRange = (from: string, to: string) => {
    const normalizedFrom = validDateOnly(from) ? from : todayDateOnly();
    const normalizedTo = validDateOnly(to) ? to : normalizedFrom;
    setFilters((current) => {
      const nextFilters = {
        ...current,
        period: { preset: "custom" as const, from: normalizedFrom, to: normalizedTo },
      };
      persistFilters(nextFilters);
      return nextFilters;
    });
  };

  const setDimension = (key: "agents" | "models" | "projects" | "branches", values: string[]) => {
    const normalized = normalizeList(values);
    setFilters((current) => {
      const nextFilters = { ...current, [key]: normalized };
      persistFilters(nextFilters);
      return nextFilters;
    });
  };

  const clearDimensions = () => {
    setFilters((current) => {
      const nextFilters = {
        ...current,
        agents: [],
        models: [],
        projects: [],
        branches: [],
      };
      persistFilters(nextFilters);
      return nextFilters;
    });
  };

  const value = useMemo(
    () => ({ filters, setPreset, setCustomRange, setDimension, clearDimensions }),
    [filters],
  );

  return <DashboardFiltersContext.Provider value={value}>{children}</DashboardFiltersContext.Provider>;
}

export function useDashboardFilters() {
  const state = useContext(DashboardFiltersContext);
  if (!state) {
    throw new Error("useDashboardFilters must be used inside DashboardFiltersProvider");
  }
  return state;
}

// Backwards-compatible hook used by existing pages.
export function usePeriod(): PeriodState {
  const { filters, setPreset, setCustomRange } = useDashboardFilters();
  return { period: filters.period, setPreset, setCustomRange };
}

// Backwards-compatible provider export.
export const PeriodProvider = DashboardFiltersProvider;
