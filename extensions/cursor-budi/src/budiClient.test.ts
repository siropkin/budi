import { describe, expect, it } from "vitest";

import {
  aggregateHealth,
  formatAggregationStatusText,
  formatAggregationTooltip,
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
