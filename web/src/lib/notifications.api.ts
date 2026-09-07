import { apiFetch, type ApiResult } from "./api";

export type Notification = {
  id: number;
  kind: "mention" | "reply" | "task_assigned" | "task_due" | "doc_shared";
  doc_id: string | null;
  doc_title: string | null;
  target_kind: string;
  target_id: string;
  actor_display_name: string | null;
  data: Record<string, unknown>;
  created_at: string;
  read: boolean;
};

export type NotificationList = {
  items: Notification[];
  next_cursor: number | null;
};

export type UnreadCount = { count: number; capped: boolean };

export const notificationsApi = {
  async list(filter: "all" | "unread" = "all", cursor?: number): Promise<ApiResult<NotificationList>> {
    const qs = new URLSearchParams({ filter });
    if (cursor !== undefined) qs.set("cursor", String(cursor));
    return apiFetch<NotificationList>(`/api/notifications?${qs.toString()}`);
  },
  async unreadCount(): Promise<ApiResult<UnreadCount>> {
    return apiFetch<UnreadCount>("/api/notifications/unread_count");
  },
  async markRead(ids: number[]): Promise<ApiResult<void>> {
    return apiFetch<void>("/api/notifications/read", { method: "POST", body: { ids } });
  },
  async markAllRead(): Promise<ApiResult<void>> {
    return apiFetch<void>("/api/notifications/read", { method: "POST", body: { all: true } });
  },
};
