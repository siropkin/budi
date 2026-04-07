import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { CalendarDays, Check, ChevronDown, X } from "lucide-react";
import type { DateRange } from "react-day-picker";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { fetchFilterOptions, fetchRegisteredProviders } from "@/lib/api";
import { formatModelName, repoName } from "@/lib/format";
import { useDashboardFilters } from "@/lib/period";
import type { DateRangePreset } from "@/lib/types";
import { cn } from "@/lib/utils";

const DATE_PRESETS: Array<{ value: DateRangePreset; label: string }> = [
  { value: "today", label: "Today" },
  { value: "last_7_days", label: "Last 7 days" },
  { value: "last_30_days", label: "Last 30 days" },
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
  className?: string;
}

function MultiSelectFilter({ label, placeholder, options, selected, onChange, renderOption, className }: MultiSelectProps) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");

  const sortedOptions = useMemo(() => {
    return [...options].sort((a, b) => {
      const left = renderOption ? renderOption(a) : a;
      const right = renderOption ? renderOption(b) : b;
      return left.localeCompare(right);
    });
  }, [options, renderOption]);

  const filteredOptions = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return sortedOptions;
    return sortedOptions.filter((value) => {
      const labelValue = renderOption ? renderOption(value) : value;
      return labelValue.toLowerCase().includes(q) || value.toLowerCase().includes(q);
    });
  }, [sortedOptions, renderOption, search]);

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
    <div className={cn("min-w-0 space-y-1", className)}>
      <p className="text-xs font-medium text-muted-foreground">{label}</p>
      <Popover modal open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" className="h-9 w-full min-w-0 justify-between gap-2 text-left">
            <span className="truncate text-sm">{summary}</span>
            <ChevronDown className="h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[280px] p-3" align="start" sideOffset={6}>
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
                      <span className="min-w-0 flex-1 truncate pr-2">{renderOption ? renderOption(value) : value}</span>
                      <Check className={cn("ml-2 h-4 w-4 shrink-0", checked ? "opacity-100" : "opacity-0")} aria-hidden="true" />
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
    queryKey: ["analytics-filter-options"],
    queryFn: ({ signal }) => fetchFilterOptions(signal),
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
  const selectedRange: DateRange | undefined = useMemo(() => {
    if (!customFrom) return undefined;
    return {
      from: parseDateOnly(customFrom) ?? undefined,
      to: customTo ? (parseDateOnly(customTo) ?? undefined) : undefined,
    };
  }, [customFrom, customTo]);
  const [customPopoverOpen, setCustomPopoverOpen] = useState(false);
  const [draftRange, setDraftRange] = useState<DateRange | undefined>(undefined);
  const [awaitingRangeStart, setAwaitingRangeStart] = useState(true);

  const options = filterOptionsQuery.data ?? { agents: [], models: [], projects: [], branches: [] };

  useEffect(() => {
    if (!filterOptionsQuery.data) return;

    const normalizeSelected = (selected: string[], available: string[]) => {
      const allowed = new Set(available);
      return selected.filter((value) => allowed.has(value));
    };
    const sameValues = (left: string[], right: string[]) =>
      left.length === right.length && left.every((value, index) => value === right[index]);

    const nextAgents = normalizeSelected(filters.agents, options.agents);
    if (!sameValues(nextAgents, filters.agents)) {
      setDimension("agents", nextAgents);
    }

    const nextModels = normalizeSelected(filters.models, options.models);
    if (!sameValues(nextModels, filters.models)) {
      setDimension("models", nextModels);
    }

    const nextProjects = normalizeSelected(filters.projects, options.projects);
    if (!sameValues(nextProjects, filters.projects)) {
      setDimension("projects", nextProjects);
    }

    const nextBranches = normalizeSelected(filters.branches, options.branches);
    if (!sameValues(nextBranches, filters.branches)) {
      setDimension("branches", nextBranches);
    }
  }, [
    filterOptionsQuery.data,
    filters.agents,
    filters.branches,
    filters.models,
    filters.projects,
    options.agents,
    options.branches,
    options.models,
    options.projects,
    setDimension,
  ]);

  useEffect(() => {
    if (!customPopoverOpen) return;
    // Initialize once when opening so in-progress clicks are not reset each render.
    setDraftRange(selectedRange);
    setAwaitingRangeStart(true);
  }, [customPopoverOpen]);

  return (
    <Card>
      <CardContent className="space-y-4 pt-4">
        <div className="flex flex-wrap items-end gap-3">
          <MultiSelectFilter
            label="Agent"
            placeholder="All agents"
            options={options.agents}
            selected={filters.agents}
            onChange={(values) => setDimension("agents", values)}
            renderOption={(value) => providerName.get(value) ?? value}
            className="flex-[1_1_170px]"
          />

          <MultiSelectFilter
            label="Model"
            placeholder="All models"
            options={options.models}
            selected={filters.models}
            onChange={(values) => setDimension("models", values)}
            renderOption={(value) => (value === "(untagged)" ? "(untagged)" : formatModelName(value))}
            className="flex-[1_1_170px]"
          />

          <MultiSelectFilter
            label="Project"
            placeholder="All projects"
            options={options.projects}
            selected={filters.projects}
            onChange={(values) => setDimension("projects", values)}
            renderOption={(value) => repoName(value)}
            className="flex-[1_1_170px]"
          />

          <MultiSelectFilter
            label="Branch"
            placeholder="All branches"
            options={options.branches}
            selected={filters.branches}
            onChange={(values) => setDimension("branches", values)}
            className="flex-[1_1_170px]"
          />

          <div className={cn("min-w-0 flex-[1_1_170px] space-y-1", filters.period.preset === "custom" && "flex-[1.8_1_340px]")}>
            <p className="text-xs font-medium text-muted-foreground">Date range</p>
            <div className="flex min-w-0 flex-wrap gap-2 sm:flex-nowrap">
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
                <SelectTrigger aria-label="Date range" className={cn("h-9 min-w-[140px] flex-1", filters.period.preset !== "custom" && "w-full")}>
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
                <Popover modal open={customPopoverOpen} onOpenChange={setCustomPopoverOpen}>
                  <PopoverTrigger asChild>
                    <Button type="button" variant="outline" className="h-9 min-w-[180px] flex-1 justify-start gap-2">
                      <CalendarDays className="h-4 w-4 shrink-0" aria-hidden="true" />
                      <span className="truncate">{formatCustomRange(customFrom, customTo)}</span>
                    </Button>
                  </PopoverTrigger>
                  <PopoverContent className="w-auto p-0" align="end" sideOffset={6}>
                    <Calendar
                      mode="range"
                      numberOfMonths={2}
                      selected={draftRange}
                      onSelect={(range, day) => {
                        if (!range?.from || Number.isNaN(range.from.getTime())) {
                          return;
                        }

                        if (awaitingRangeStart) {
                          const nextStart = day && !Number.isNaN(day.getTime()) ? day : range.from;
                          setDraftRange({ from: nextStart, to: undefined });
                          setAwaitingRangeStart(false);
                          return;
                        }

                        setDraftRange(range);
                        if (!range.to) return;
                        const from = toDateOnly(range.from);
                        const to = toDateOnly(range.to);
                        setCustomRange(from, to);
                        setCustomPopoverOpen(false);
                      }}
                      defaultMonth={draftRange?.from ?? selectedRange?.from}
                    />
                  </PopoverContent>
                </Popover>
              ) : null}
            </div>
          </div>
        </div>

        {(filters.agents.length > 0 || filters.models.length > 0 || filters.projects.length > 0 || filters.branches.length > 0) ? (
          <div className="flex flex-wrap items-center gap-2">
            {[
              ...filters.agents,
              ...filters.models,
              ...filters.projects.map((project) => repoName(project)),
              ...filters.branches,
            ]
              .slice(0, 6)
              .map((value, index) => (
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
