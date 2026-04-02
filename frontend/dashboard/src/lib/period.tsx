import { createContext, useContext, useMemo, useState } from "react";
import type { DateRangePreset, DateRangeSelection } from "@/lib/types";

interface PeriodState {
  period: DateRangeSelection;
  setPreset: (preset: DateRangePreset) => void;
  setCustomRange: (from: string, to: string) => void;
}

const PeriodContext = createContext<PeriodState | null>(null);

const QUICK_PRESETS: DateRangePreset[] = ["today", "month_to_date", "last_7_days", "last_30_days", "last_month", "custom"];
const DATE_INPUT_PATTERN = /^\d{4}-\d{2}-\d{2}$/;

function toDateInput(date: Date): string {
  const year = String(date.getFullYear());
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function addDays(date: Date, days: number): Date {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate() + days);
}

function parseDateInput(value: string): Date | null {
  if (!DATE_INPUT_PATTERN.test(value)) return null;
  const [yearStr, monthStr, dayStr] = value.split("-");
  const year = Number(yearStr);
  const month = Number(monthStr);
  const day = Number(dayStr);
  if (!Number.isInteger(year) || !Number.isInteger(month) || !Number.isInteger(day)) return null;
  const parsed = new Date(year, month - 1, day);
  if (parsed.getFullYear() !== year || parsed.getMonth() !== month - 1 || parsed.getDate() !== day) return null;
  return parsed;
}

function defaultCustomRange(): DateRangeSelection {
  const today = new Date();
  return {
    preset: "custom",
    from: toDateInput(addDays(today, -6)),
    to: toDateInput(today),
  };
}

function validPreset(value: unknown): value is DateRangePreset {
  return typeof value === "string" && QUICK_PRESETS.includes(value as DateRangePreset);
}

function normalizeRange(value: unknown): DateRangeSelection | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<DateRangeSelection>;
  if (!validPreset(candidate.preset)) return null;

  if (candidate.preset !== "custom") {
    return { preset: candidate.preset };
  }

  if (typeof candidate.from !== "string" || typeof candidate.to !== "string") {
    return defaultCustomRange();
  }

  const fromDate = parseDateInput(candidate.from);
  const toDate = parseDateInput(candidate.to);
  if (!fromDate || !toDate || candidate.from > candidate.to) {
    return defaultCustomRange();
  }

  return {
    preset: "custom",
    from: candidate.from,
    to: candidate.to,
  };
}

function legacyPeriodRange(value: string | null): DateRangeSelection | null {
  switch (value) {
    case "today":
      return { preset: "today" };
    case "week":
      return { preset: "last_7_days" };
    case "month":
      return { preset: "month_to_date" };
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
    const nextPeriod = preset === "custom" ? defaultCustomRange() : { preset };
    persistPeriod(nextPeriod);
    setPeriodState(nextPeriod);
  };

  const setCustomRange = (from: string, to: string) => {
    if (!parseDateInput(from) || !parseDateInput(to) || from > to) {
      return;
    }
    const nextPeriod: DateRangeSelection = { preset: "custom", from, to };
    persistPeriod(nextPeriod);
    setPeriodState(nextPeriod);
  };

  const value = useMemo(() => ({ period, setPreset, setCustomRange }), [period]);

  return <PeriodContext.Provider value={value}>{children}</PeriodContext.Provider>;
}

export function usePeriod() {
  const state = useContext(PeriodContext);
  if (!state) {
    throw new Error("usePeriod must be used inside PeriodProvider");
  }
  return state;
}
