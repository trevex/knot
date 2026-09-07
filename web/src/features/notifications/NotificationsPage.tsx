/**
 * /notifications — the full inbox.
 *
 * Rows carrying a doc_id navigate to that document; a comment-anchored one
 * appends ?thread=<id> so the comment sidebar opens on the right thread.
 */
import { useState } from "react";
import { useNavigate } from "react-router-dom";

import type { Notification } from "../../lib/notifications.api";
import { KIND_LABEL, targetPath } from "./notificationTarget";
import { useMarkRead, useNotificationList } from "./useNotifications";

export default function NotificationsPage() {
  const [filter, setFilter] = useState<"all" | "unread">("all");
  const list = useNotificationList(filter);
  const markRead = useMarkRead();
  const nav = useNavigate();

  function open(n: Notification) {
    if (!n.read) markRead.mutate([n.id]);
    const path = targetPath(n);
    if (path) void nav(path);
  }

  const items = list.data?.items ?? [];

  return (
    <div data-testid="notifications-page" className="max-w-3xl mx-auto px-6 py-8">
      <div className="flex items-center gap-2 mb-4">
        <h1 className="text-lg font-semibold text-fg flex-1">Inbox</h1>
        <button
          type="button"
          data-testid="notifications-filter-all"
          aria-pressed={filter === "all"}
          onClick={() => setFilter("all")}
          className={`h-7 px-2 rounded text-[13px] ${filter === "all" ? "bg-muted text-fg" : "text-fg-muted hover:text-fg"}`}
        >
          All
        </button>
        <button
          type="button"
          data-testid="notifications-filter-unread"
          aria-pressed={filter === "unread"}
          onClick={() => setFilter("unread")}
          className={`h-7 px-2 rounded text-[13px] ${filter === "unread" ? "bg-muted text-fg" : "text-fg-muted hover:text-fg"}`}
        >
          Unread
        </button>
        <button
          type="button"
          data-testid="notifications-mark-all"
          onClick={() => markRead.mutate("all")}
          className="h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted"
        >
          Mark all read
        </button>
      </div>

      {list.isPending && <p className="text-[13px] text-fg-muted">Loading…</p>}
      {!list.isPending && items.length === 0 && (
        <p className="text-[13px] text-fg-muted">Nothing here yet.</p>
      )}

      <ul className="flex flex-col gap-1">
        {items.map((n) => (
          <li key={n.id}>
            <button
              type="button"
              data-testid="notification-row"
              data-kind={n.kind}
              data-read={n.read ? "true" : "false"}
              onClick={() => open(n)}
              className="w-full text-left px-3 py-2 rounded hover:bg-muted transition-colors ease-swift duration-150"
            >
              <div className="text-[13px] text-fg">
                {!n.read && (
                  <span aria-hidden className="inline-block h-2 w-2 rounded-full bg-accent mr-2" />
                )}
                <strong className="font-semibold">{n.actor_display_name ?? "knot"}</strong>{" "}
                {KIND_LABEL[n.kind]}
                {n.doc_title ? <> in <span className="text-fg-muted">{n.doc_title}</span></> : null}
              </div>
              {typeof n.data.excerpt === "string" && (
                <div className="text-[12px] text-fg-muted truncate">{n.data.excerpt}</div>
              )}
              <div className="text-[11px] text-fg-muted/80">
                {new Date(n.created_at).toLocaleString()}
              </div>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
