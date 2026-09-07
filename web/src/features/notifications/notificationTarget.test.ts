import { describe, expect, it } from "vitest";

import type { Notification } from "../../lib/notifications.api";
import { notificationText, targetPath } from "./notificationTarget";

function base(overrides: Partial<Notification>): Notification {
  return {
    id: 1,
    kind: "mention",
    doc_id: null,
    doc_title: null,
    target_kind: "comment",
    target_id: "1",
    actor_display_name: null,
    data: {},
    created_at: new Date().toISOString(),
    read: false,
    ...overrides,
  };
}

describe("notificationText", () => {
  // Pins Finding 2: task_due is null-actor by design (system-generated),
  // and rendering `actor_display_name ?? "knot"` made the product
  // announce itself — "knot task is overdue". A null actor must render as
  // a plain statement, not an attribution to a fake user named "knot".
  it("renders task_due with no actor as a statement, not an attribution to 'knot'", () => {
    const n = base({ kind: "task_due", actor_display_name: null });
    expect(notificationText(n)).toEqual({ actor: null, label: "Task is overdue" });
  });

  // task_assigned is null-actor on the live-editing path (design spec §2:
  // a co-edit that assigns a task has no single "who assigned this"
  // answer). The spec explicitly says this should render as "task
  // assigned to you" with no actor name — pin that.
  it("renders task_assigned with no actor as a statement", () => {
    const n = base({ kind: "task_assigned", actor_display_name: null });
    expect(notificationText(n)).toEqual({ actor: null, label: "Task assigned to you" });
  });

  it("still attributes to the actor when one is present", () => {
    const n = base({ kind: "mention", actor_display_name: "Alice" });
    expect(notificationText(n)).toEqual({ actor: "Alice", label: "mentioned you" });
  });

  it("attributes task_assigned to the actor when a real one caused it", () => {
    const n = base({ kind: "task_assigned", actor_display_name: "Alice" });
    expect(notificationText(n)).toEqual({ actor: "Alice", label: "assigned you a task" });
  });
});

describe("targetPath", () => {
  it("returns null with no doc_id", () => {
    expect(targetPath(base({ doc_id: null }))).toBeNull();
  });

  it("appends the thread query param when data.thread_id is a string", () => {
    const n = base({ doc_id: "d1", data: { thread_id: "t1" } });
    expect(targetPath(n)).toBe("/doc/d1?thread=t1");
  });

  it("links straight to the doc with no thread_id", () => {
    const n = base({ doc_id: "d1", data: {} });
    expect(targetPath(n)).toBe("/doc/d1");
  });
});
