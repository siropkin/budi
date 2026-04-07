import type { DashboardFilters } from "@/lib/types";
import { periodLabel } from "@/lib/format";

export interface CsvColumn<T> {
  header: string;
  value: (row: T) => string | number | null | undefined;
}

function escapeCsvValue(raw: unknown): string {
  if (raw == null) return "";
  const text = String(raw);
  if (/[",\n\r]/.test(text)) {
    return `"${text.replace(/"/g, '""')}"`;
  }
  return text;
}

export function toCsv<T>(rows: T[], columns: CsvColumn<T>[]): string {
  const header = columns.map((col) => escapeCsvValue(col.header)).join(",");
  const body = rows.map((row) =>
    columns.map((col) => escapeCsvValue(col.value(row))).join(","),
  );
  return [header, ...body].join("\n");
}

export function downloadCsv(csv: string, filename: string): void {
  const blob = new Blob([csv], { type: "text/csv;charset=utf-8;" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.style.display = "none";
  document.body.appendChild(link);
  link.click();
  document.body.removeChild(link);
  URL.revokeObjectURL(url);
}

export function buildExportFilename(table: string, filters?: DashboardFilters): string {
  const date = new Date().toISOString().slice(0, 10);
  const parts = [table];

  if (filters) {
    const period = periodLabel(filters.period).toLowerCase().replace(/\s+/g, "-");
    parts.push(period);
  }

  parts.push(date);
  return `${parts.join("_")}.csv`;
}
