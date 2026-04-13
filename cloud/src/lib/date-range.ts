import { type DateRange } from "@/lib/dal";
import { format, subDays } from "date-fns";

/** Build a DateRange from the `days` search param (default 30). */
export function dateRangeFromDays(days: string | undefined): DateRange {
  const n = Number(days) || 30;
  const to = new Date();
  const from = subDays(to, n);
  return {
    from: format(from, "yyyy-MM-dd"),
    to: format(to, "yyyy-MM-dd"),
  };
}
