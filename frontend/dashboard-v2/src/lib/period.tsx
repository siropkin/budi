import { createContext, useContext, useMemo, useState } from "react";
import type { Period } from "@/lib/types";

interface PeriodState {
  period: Period;
  setPeriod: (period: Period) => void;
}

const PeriodContext = createContext<PeriodState | null>(null);

function getInitialPeriod(): Period {
  const value = window.localStorage.getItem("budi_period");
  if (value === "today" || value === "week" || value === "month" || value === "all") {
    return value;
  }
  return "today";
}

export function PeriodProvider({ children }: { children: React.ReactNode }) {
  const [period, setPeriodState] = useState<Period>(getInitialPeriod);

  const setPeriod = (nextPeriod: Period) => {
    window.localStorage.setItem("budi_period", nextPeriod);
    setPeriodState(nextPeriod);
  };

  const value = useMemo(() => ({ period, setPeriod }), [period]);

  return <PeriodContext.Provider value={value}>{children}</PeriodContext.Provider>;
}

export function usePeriod() {
  const state = useContext(PeriodContext);
  if (!state) {
    throw new Error("usePeriod must be used inside PeriodProvider");
  }
  return state;
}
