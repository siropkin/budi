import { createContext, useContext, useMemo, useState } from "react";
import type { DateRangePreset, DateRangeSelection } from "@/lib/types";

interface PeriodState {
  period: DateRangeSelection;
  setPreset: (preset: DateRangePreset) => void;
}

const PeriodContext = createContext<PeriodState | null>(null);

const QUICK_PRESETS: DateRangePreset[] = ["today", "last_7_days", "last_30_days"];

function validPreset(value: unknown): value is DateRangePreset {
  return typeof value === "string" && QUICK_PRESETS.includes(value as DateRangePreset);
}

function normalizeRange(value: unknown): DateRangeSelection | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<DateRangeSelection>;
  if (!validPreset(candidate.preset)) return null;
  return { preset: candidate.preset };
}

function legacyPeriodRange(value: string | null): DateRangeSelection | null {
  switch (value) {
    case "today":
      return { preset: "today" };
    case "week":
      return { preset: "last_7_days" };
    case "month":
    case "all":
      return { preset: "last_30_days" };
    default:
      return null;
  }
}

function persistPeriod(nextPeriod: DateRangeSelection) {
  window.localStorage.setItem("budi_period_range", JSON.stringify(nextPeriod));
  window.localStorage.removeItem("budi_period");
}

function getInitialPeriod(): DateRangeSelection {
  const stored = window.localStorage.getItem("budi_period_range");
  if (stored) {
    try {
      const parsed = normalizeRange(JSON.parse(stored) as unknown);
      if (parsed) return parsed;
    } catch {
      // Ignore malformed storage and fall back to defaults.
    }
  }

  const legacy = legacyPeriodRange(window.localStorage.getItem("budi_period"));
  if (legacy) return legacy;

  return { preset: "today" };
}

export function PeriodProvider({ children }: { children: React.ReactNode }) {
  const [period, setPeriodState] = useState<DateRangeSelection>(getInitialPeriod);

  const setPreset = (preset: DateRangePreset) => {
    const nextPeriod = { preset };
    persistPeriod(nextPeriod);
    setPeriodState(nextPeriod);
  };

  const value = useMemo(() => ({ period, setPreset }), [period]);

  return <PeriodContext.Provider value={value}>{children}</PeriodContext.Provider>;
}

export function usePeriod() {
  const state = useContext(PeriodContext);
  if (!state) {
    throw new Error("usePeriod must be used inside PeriodProvider");
  }
  return state;
}
