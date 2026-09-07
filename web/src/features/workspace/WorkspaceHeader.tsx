import { Bell, CheckSquare, LayoutTemplate, Search, Settings, Users } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";

import { useSession } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";
import { NotificationDropdown } from "../notifications/NotificationDropdown";
import { useUnreadCount, formatBadge } from "../notifications/useNotifications";

export function WorkspaceHeader() {
  const session = useSession();
  const user = session.data && "ok" in session.data ? session.data.ok : null;
  const openPalette = useUi((s) => s.openPalette);
  const unread = useUnreadCount();
  const badge = unread.data ? formatBadge(unread.data.count, unread.data.capped) : "";
  const [inboxOpen, setInboxOpen] = useState(false);

  const initial = (user?.display_name ?? "?").slice(0, 1).toUpperCase();

  return (
    <div className="px-3 pt-3 pb-2 border-b border-border">
      <div className="flex items-center gap-2 mb-3">
        <div
          aria-hidden
          className="h-7 w-7 rounded bg-accent text-accent-fg flex items-center justify-center text-[13px] font-semibold shrink-0"
        >
          {initial}
        </div>
        <div className="min-w-0 flex-1">
          <div className="text-[13px] font-semibold text-fg truncate">{user?.display_name ?? "Workspace"}</div>
          <div className="text-[11px] text-fg-muted truncate">{user?.email ?? ""}</div>
        </div>
      </div>
      <button
        type="button"
        data-testid="sidebar-search"
        onClick={openPalette}
        className="w-full flex items-center gap-2 h-8 px-2 rounded bg-muted text-fg-muted hover:text-fg transition-colors ease-swift duration-150"
      >
        <Search size={14} aria-hidden />
        <span className="text-[13px]">Search…</span>
        <span className="ml-auto text-[11px] text-fg-muted/80">⌘K</span>
      </button>
      <nav className="mt-2 flex flex-col gap-0.5">
        <div className="relative">
          <button
            type="button"
            data-testid="sidebar-inbox"
            onClick={() => setInboxOpen((v) => !v)}
            className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
          >
            <Bell size={14} aria-hidden /> Inbox
            {badge !== "" && (
              <span
                data-testid="inbox-badge"
                className="ml-auto min-w-[18px] px-1 h-[18px] rounded-full bg-accent text-accent-fg text-[11px] font-semibold inline-flex items-center justify-center"
              >
                {badge}
              </span>
            )}
          </button>
          {inboxOpen && <NotificationDropdown onClose={() => setInboxOpen(false)} />}
        </div>
        <Link
          to="/tasks"
          data-testid="sidebar-tasks"
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <CheckSquare size={14} aria-hidden /> Tasks
        </Link>
        <Link
          to="/templates"
          data-testid="sidebar-templates"
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <LayoutTemplate size={14} aria-hidden /> Templates
        </Link>
        <Link
          to="/members"
          data-testid="sidebar-members"
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <Users size={14} aria-hidden /> Members
        </Link>
        <Link
          to="/settings"
          data-testid="sidebar-settings"
          className="inline-flex items-center gap-2 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
        >
          <Settings size={14} aria-hidden /> Settings
        </Link>
      </nav>
    </div>
  );
}
