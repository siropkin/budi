import { describe, expect, it } from "vitest";

import { CONTRACT_VERSION } from "./sessionStore";

describe("sessionStore contract", () => {
  it("contract version is 1", () => {
    expect(CONTRACT_VERSION).toBe(1);
  });
});
