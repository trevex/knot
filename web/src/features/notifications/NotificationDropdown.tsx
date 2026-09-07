/**
 * The sidebar's Inbox dropdown — the most recent 10 notifications, newest
 * first (the server orders by id DESC; nothing here re-sorts by read
 * state). Clicking a row marks it read and navigates; "Open inbox" goes to
 * the full page. Dismissal (outside click / Escape) is owned by
 * WorkspaceHeader, which holds both the trigger button and this dropdown
 * under one container ref — see the effect there.
 */
import { useNavigate } from "react-router-dom";

import type { Notification } from "../../lib/notifications.api";
import { KIND_LABEL, targetPath } from "./notificationTarget";
import { useMarkRead, useNotificationList } from "./useNotifications";

export function NotificationDropdown({ onClose }: { onClose: () => void }) {
  const list = useNotificationList("all");
  const markRead = useMarkRead();
  const nav = useNavigate();

  function open(n: Notification) {
    if (!n.read) markRead.mutate([n.id]);
    const path = targetPath(n);
    onClose();
    if (path) void nav(path);
  }

  const items = (list.data?.items ?? []).slice(0, 10);

  return (
    <div
      role="menu"
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
              role="menuitem"
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
        role="menuitem"
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
