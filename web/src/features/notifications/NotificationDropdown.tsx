/**
 * The sidebar's Inbox dropdown — the last 10, unread first by recency.
 * Clicking a row marks it read and navigates; "Open inbox" goes to the
 * full page. Dismisses on an outside click or Escape, matching the
 * ContextMenu convention used elsewhere in the app.
 */
import { useEffect, useRef } from "react";
import { useNavigate } from "react-router-dom";

import type { Notification } from "../../lib/notifications.api";
import { KIND_LABEL, targetPath } from "./notificationTarget";
import { useMarkRead, useNotificationList } from "./useNotifications";

export function NotificationDropdown({ onClose }: { onClose: () => void }) {
  const list = useNotificationList("all");
  const markRead = useMarkRead();
  const nav = useNavigate();
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      const target = e.target as HTMLElement;
      if (ref.current?.contains(target)) return;
      // The trigger button owns its own toggle; without this guard a click
      // that re-closes an open dropdown would fire this mousedown listener
      // first (closing it), then the button's click handler (reopening it).
      if (target.closest('[data-testid="sidebar-inbox"]')) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  function open(n: Notification) {
    if (!n.read) markRead.mutate([n.id]);
    const path = targetPath(n);
    onClose();
    if (path) void nav(path);
  }

  const items = (list.data?.items ?? []).slice(0, 10);

  return (
    <div
      ref={ref}
      data-testid="notifications-dropdown"
      className="absolute left-2 right-2 top-full z-50 mt-1 rounded-md border border-border bg-surface shadow-lg overflow-hidden"
    >
      {items.length === 0 && (
        <p className="px-3 py-3 text-[12px] text-fg-muted m-0">Nothing here yet.</p>
      )}
      <ul className="m-0 p-0 list-none max-h-[320px] overflow-y-auto">
        {items.map((n) => (
          <li key={n.id}>
            <button
              type="button"
              data-testid="notification-row"
              data-kind={n.kind}
              data-read={n.read ? "true" : "false"}
              onClick={() => open(n)}
              className="w-full text-left px-3 py-2 text-[12px] text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              {!n.read && (
                <span aria-hidden className="inline-block h-2 w-2 rounded-full bg-accent mr-2" />
              )}
              <strong className="font-semibold">{n.actor_display_name ?? "knot"}</strong>{" "}
              {KIND_LABEL[n.kind]}
              {n.doc_title ? <span className="text-fg-muted"> · {n.doc_title}</span> : null}
            </button>
          </li>
        ))}
      </ul>
      <button
        type="button"
        data-testid="notifications-open-inbox"
        onClick={() => {
          onClose();
          void nav("/notifications");
        }}
        className="w-full text-left px-3 py-2 text-[12px] text-fg-muted hover:text-fg hover:bg-muted border-t border-border"
      >
        Open inbox
      </button>
    </div>
  );
}
