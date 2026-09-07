import { describe, expect, it } from "vitest";

import { formatBadge, unwrapResult } from "./useNotifications";

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

describe("unwrapResult", () => {
  it("returns the ok payload untouched", () => {
    expect(unwrapResult({ ok: 42 })).toBe(42);
  });

  it("throws a value carrying the numeric HTTP status, so queryClient's 4xx-skip retry predicate can see it", () => {
    const res = { error: { code: "forbidden", message: "nope", details: {}, status: 403 } };

    let caught: unknown;
    try {
      unwrapResult(res);
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(Error);
    expect((caught as Error).message).toBe("nope");
    // This is the load-bearing assertion: queryClient.ts's retry predicate
    // does `"status" in error` then reads a numeric `status`. A refactor
    // back to a bare `new Error(message)` here would make this fail instead
    // of silently reintroducing the retry storm on every 401/403/404.
    expect((caught as { status: unknown }).status).toBe(403);
    expect((caught as { code: unknown }).code).toBe("forbidden");
  });
});
