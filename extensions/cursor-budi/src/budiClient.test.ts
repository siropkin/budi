import { describe, expect, it } from "vitest";

import {
  aggregateHealth,
  formatAggregationStatusText,
  formatAggregationTooltip,
  splitSessionsByDay,
  MIN_API_VERSION,
  type SessionListEntry,
} from "./budiClient";

describe("aggregateHealth", () => {
  it("counts health states across sessions", () => {
    const sessions: SessionListEntry[] = [
      {
        session_id: "s1",
        message_count: 1,
        cost_cents: 10,
        provider: "cursor",
        health_state: "green",
      },
      {
        session_id: "s2",
        message_count: 2,
        cost_cents: 30,
        provider: "cursor",
        health_state: "yellow",
      },
      {
        session_id: "s3",
        message_count: 3,
        cost_cents: 50,
        provider: "cursor",
        health_state: "red",
      },
      { session_id: "s4", message_count: 4, cost_cents: 70, provider: "cursor" },
    ];

    expect(aggregateHealth(sessions)).toEqual({
      green: 2,
      yellow: 1,
      red: 1,
      total: 4,
    });
  });
});

describe("aggregation status formatting", () => {
  it("renders compact status text", () => {
    const text = formatAggregationStatusText({ green: 2, yellow: 1, red: 0, total: 3 });
    expect(text).toContain("budi");
    expect(text).toContain("2");
    expect(text).toContain("1");
  });

  it("renders tooltip with cost and state details", () => {
    const tooltip = formatAggregationTooltip({ green: 1, yellow: 0, red: 1, total: 2 }, 12.34);
    expect(tooltip).toContain("Today's sessions: 2");
    expect(tooltip).toContain("$12.34");
    expect(tooltip).toContain("needs attention");
  });
});

describe("splitSessionsByDay", () => {
  it("groups sessions into today and yesterday", () => {
    const now = new Date();
    const twoHoursAgo = new Date(now.getTime() - 2 * 60 * 60 * 1000).toISOString();
    const yesterday = new Date(now.getTime() - 26 * 60 * 60 * 1000).toISOString();
    const threeDaysAgo = new Date(now.getTime() - 72 * 60 * 60 * 1000).toISOString();

    const sessions: SessionListEntry[] = [
      {
        session_id: "today",
        started_at: twoHoursAgo,
        message_count: 1,
        cost_cents: 1,
        provider: "cursor",
      },
      {
        session_id: "yesterday",
        started_at: yesterday,
        message_count: 1,
        cost_cents: 1,
        provider: "cursor",
      },
      {
        session_id: "old",
        started_at: threeDaysAgo,
        message_count: 1,
        cost_cents: 1,
        provider: "cursor",
      },
      { session_id: "missing", message_count: 1, cost_cents: 1, provider: "cursor" },
    ];

    const grouped = splitSessionsByDay(sessions);
    expect(grouped.today.map((s) => s.session_id)).toEqual(["today"]);
    expect(grouped.yesterday.map((s) => s.session_id)).toEqual(["yesterday"]);
  });
});

describe("MIN_API_VERSION", () => {
  it("is at least 1", () => {
    expect(MIN_API_VERSION).toBeGreaterThanOrEqual(1);
  });
});
