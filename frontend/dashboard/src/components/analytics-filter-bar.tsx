import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { CalendarDays, Check, ChevronDown, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { fetchFilterOptions, fetchRegisteredProviders } from "@/lib/api";
import { formatModelName } from "@/lib/format";
import { useDashboardFilters } from "@/lib/period";
import type { DateRangePreset } from "@/lib/types";
import { cn } from "@/lib/utils";

const DATE_PRESETS: Array<{ value: DateRangePreset; label: string }> = [
  { value: "today", label: "Today" },
  { value: "last_7_days", label: "Last 7 days" },
  { value: "last_30_days", label: "Last 30 days" },
  { value: "all", label: "All" },
  { value: "custom", label: "Custom" },
];

function parseDateOnly(value: string): Date | null {
  const match = value.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!match) return null;
  const year = Number(match[1]);
  const month = Number(match[2]) - 1;
  const day = Number(match[3]);
  const parsed = new Date(year, month, day);
  if (Number.isNaN(parsed.getTime())) return null;
  return parsed;
}

function toDateOnly(value: Date): string {
  const year = value.getFullYear();
  const month = `${value.getMonth() + 1}`.padStart(2, "0");
  const day = `${value.getDate()}`.padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function formatCustomRange(from?: string, to?: string): string {
  if (!from || !to) return "Pick range";
  const fromDate = parseDateOnly(from);
  const toDate = parseDateOnly(to);
  if (!fromDate || !toDate) return "Pick range";

  const fromLabel = fromDate.toLocaleDateString([], { month: "short", day: "numeric" });
  const toLabel = toDate.toLocaleDateString([], { month: "short", day: "numeric" });
  return `${fromLabel} - ${toLabel}`;
}

function todayDateOnly(): string {
  return toDateOnly(new Date());
}

interface MultiSelectProps {
  label: string;
  placeholder: string;
  options: string[];
  selected: string[];
  onChange: (values: string[]) => void;
  renderOption?: (value: string) => string;
}

function MultiSelectFilter({ label, placeholder, options, selected, onChange, renderOption }: MultiSelectProps) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");

  const mergedOptions = useMemo(() => {
    const all = new Set<string>([...options, ...selected]);
    return Array.from(all).sort((a, b) => {
      const left = renderOption ? renderOption(a) : a;
      const right = renderOption ? renderOption(b) : b;
      return left.localeCompare(right);
    });
  }, [options, selected, renderOption]);

  const filteredOptions = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return mergedOptions;
    return mergedOptions.filter((value) => {
      const labelValue = renderOption ? renderOption(value) : value;
      return labelValue.toLowerCase().includes(q) || value.toLowerCase().includes(q);
    });
  }, [mergedOptions, renderOption, search]);

  const toggle = (value: string) => {
    if (selected.includes(value)) {
      onChange(selected.filter((entry) => entry !== value));
      return;
    }
    onChange([...selected, value]);
  };

  const summary =
    selected.length === 0
      ? placeholder
      : selected.length === 1
        ? (renderOption ? renderOption(selected[0]) : selected[0])
        : `${selected.length} selected`;

  return (
    <div className="space-y-1">
      <p className="text-xs font-medium text-muted-foreground">{label}</p>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" className="h-9 w-full justify-between gap-2 text-left md:min-w-[190px]">
            <span className="truncate text-sm">{summary}</span>
            <ChevronDown className="h-4 w-4 text-muted-foreground" aria-hidden="true" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[280px] p-3" align="start">
          <div className="space-y-2">
            <Input
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder={`Search ${label.toLowerCase()}...`}
              className="h-8"
            />
            <div className="max-h-52 overflow-y-auto rounded-md border border-border">
              {filteredOptions.length === 0 ? (
                <p className="px-3 py-2 text-sm text-muted-foreground">No matches</p>
              ) : (
                filteredOptions.map((value) => {
                  const checked = selected.includes(value);
                  return (
                    <button
                      key={value}
                      type="button"
                      className="flex w-full items-center justify-between px-3 py-2 text-left text-sm hover:bg-muted"
                      onClick={() => toggle(value)}
                    >
                      <span className="truncate">{renderOption ? renderOption(value) : value}</span>
                      <Check className={cn("ml-3 h-4 w-4", checked ? "opacity-100" : "opacity-0")} aria-hidden="true" />
                    </button>
                  );
                })
              )}
            </div>
            <div className="flex items-center justify-between">
              <span className="text-xs text-muted-foreground">{selected.length} selected</span>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                disabled={selected.length === 0}
                onClick={() => onChange([])}
              >
                Clear
              </Button>
            </div>
          </div>
        </PopoverContent>
      </Popover>
    </div>
  );
}

export function AnalyticsFilterBar() {
  const { filters, setPreset, setCustomRange, setDimension, clearDimensions } = useDashboardFilters();

  const providersQuery = useQuery({
    queryKey: ["registered-providers"],
    queryFn: ({ signal }) => fetchRegisteredProviders(signal),
    staleTime: 60_000,
  });

  const filterOptionsQuery = useQuery({
    queryKey: ["analytics-filter-options", filters.period],
    queryFn: ({ signal }) => fetchFilterOptions(filters, signal),
    staleTime: 60_000,
  });

  const providerName = useMemo(() => {
    const map = new Map<string, string>();
    for (const provider of providersQuery.data ?? []) {
      map.set(provider.name, provider.display_name);
    }
    return map;
  }, [providersQuery.data]);

  const customFrom = filters.period.preset === "custom" ? filters.period.from : undefined;
  const customTo = filters.period.preset === "custom" ? filters.period.to : undefined;
  const selectedRange = customFrom && customTo ? { from: parseDateOnly(customFrom) ?? undefined, to: parseDateOnly(customTo) ?? undefined } : undefined;

  const options = filterOptionsQuery.data ?? { agents: [], models: [], projects: [], branches: [] };

  return (
    <Card>
      <CardContent className="space-y-4 pt-4">
        <div className="grid gap-3 lg:grid-cols-5">
          <div className="space-y-1">
            <p className="text-xs font-medium text-muted-foreground">Date range</p>
            <div className="flex gap-2">
              <Select
                value={filters.period.preset}
                onValueChange={(value) => {
                  const preset = value as DateRangePreset;
                  setPreset(preset);
                  if (preset === "custom" && (!customFrom || !customTo)) {
                    const today = todayDateOnly();
                    setCustomRange(today, today);
                  }
                }}
              >
                <SelectTrigger aria-label="Date range" className="h-9 w-full md:min-w-[160px]">
                  <SelectValue placeholder="Select range" />
                </SelectTrigger>
                <SelectContent>
                  {DATE_PRESETS.map((preset) => (
                    <SelectItem key={preset.value} value={preset.value}>
                      {preset.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              {filters.period.preset === "custom" ? (
                <Popover>
                  <PopoverTrigger asChild>
                    <Button type="button" variant="outline" className="h-9 justify-start gap-2 whitespace-nowrap">
                      <CalendarDays className="h-4 w-4" aria-hidden="true" />
                      {formatCustomRange(customFrom, customTo)}
                    </Button>
                  </PopoverTrigger>
                  <PopoverContent className="w-auto p-0" align="start">
                    <Calendar
                      mode="range"
                      numberOfMonths={2}
                      selected={selectedRange}
                      onSelect={(range) => {
                        if (!range?.from) return;
                        const from = toDateOnly(range.from);
                        const to = toDateOnly(range.to ?? range.from);
                        setCustomRange(from, to);
                      }}
                      defaultMonth={selectedRange?.from}
                    />
                  </PopoverContent>
                </Popover>
              ) : null}
            </div>
          </div>

          <MultiSelectFilter
            label="Agent"
            placeholder="All agents"
            options={options.agents}
            selected={filters.agents}
            onChange={(values) => setDimension("agents", values)}
            renderOption={(value) => providerName.get(value) ?? value}
          />

          <MultiSelectFilter
            label="Model"
            placeholder="All models"
            options={options.models}
            selected={filters.models}
            onChange={(values) => setDimension("models", values)}
            renderOption={(value) => (value === "(untagged)" ? "(untagged)" : formatModelName(value))}
          />

          <MultiSelectFilter
            label="Project"
            placeholder="All projects"
            options={options.projects}
            selected={filters.projects}
            onChange={(values) => setDimension("projects", values)}
          />

          <MultiSelectFilter
            label="Branch"
            placeholder="All branches"
            options={options.branches}
            selected={filters.branches}
            onChange={(values) => setDimension("branches", values)}
          />
        </div>

        {(filters.agents.length > 0 || filters.models.length > 0 || filters.projects.length > 0 || filters.branches.length > 0) ? (
          <div className="flex flex-wrap items-center gap-2">
            {[...filters.agents, ...filters.models, ...filters.projects, ...filters.branches].slice(0, 6).map((value, index) => (
              <Badge key={`${value}-${index}`} variant="outline" className="font-normal text-muted-foreground">
                {value}
              </Badge>
            ))}
            <Button type="button" variant="ghost" size="sm" className="h-7 gap-1" onClick={clearDimensions}>
              <X className="h-3.5 w-3.5" aria-hidden="true" />
              Clear filters
            </Button>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}
