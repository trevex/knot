import { describe, expect, it } from "vitest";

import { formatBadge } from "./useNotifications";

describe("formatBadge", () => {
  it("renders a plain count under the cap", () => {
    expect(formatBadge(3, false)).toBe("3");
  });

  it("renders 99+ once capped", () => {
    expect(formatBadge(100, true)).toBe("99+");
  });

  it("renders an empty string at zero so the badge can hide", () => {
    expect(formatBadge(0, false)).toBe("");
  });
});
