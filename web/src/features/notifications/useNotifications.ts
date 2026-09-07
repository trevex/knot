import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { notificationsApi, type NotificationList, type UnreadCount } from "../../lib/notifications.api";

/** Delivery is polling, not push — see the spec's §6. Window focus covers
 *  most of the perceived latency; this is the ceiling. */
export const POLL_INTERVAL_MS = 30_000;

export function formatBadge(count: number, capped: boolean): string {
  if (count <= 0) return "";
  return capped ? "99+" : String(count);
}

export function useUnreadCount() {
  return useQuery<UnreadCount>({
    queryKey: ["notifications", "unread_count"],
    queryFn: async () => {
      const res = await notificationsApi.unreadCount();
      if ("error" in res) throw new Error(res.error.message);
      return res.ok;
    },
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: true,
  });
}

export function useNotificationList(filter: "all" | "unread") {
  return useQuery<NotificationList>({
    queryKey: ["notifications", "list", filter],
    queryFn: async () => {
      const res = await notificationsApi.list(filter);
      if ("error" in res) throw new Error(res.error.message);
      return res.ok;
    },
  });
}

export function useMarkRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (ids: number[] | "all") => {
      const res = ids === "all"
        ? await notificationsApi.markAllRead()
        : await notificationsApi.markRead(ids);
      if ("error" in res) throw new Error(res.error.message);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
  });
}
