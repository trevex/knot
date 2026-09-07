import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import type { ApiResult } from "../../lib/api";
import { notificationsApi, type NotificationList, type UnreadCount } from "../../lib/notifications.api";

/** Delivery is polling, not push — see the spec's §6. Window focus covers
 *  most of the perceived latency; this is the ceiling. */
export const POLL_INTERVAL_MS = 30_000;

export function formatBadge(count: number, capped: boolean): string {
  if (count <= 0) return "";
  return capped ? "99+" : String(count);
}

/** Unwraps an ApiResult, throwing on error. The thrown value carries the
 *  HTTP `status` (and `code`) from the API error so that
 *  `queryClient`'s retry predicate — which keys off `"status" in error` —
 *  can see it and correctly skip retries on 4xx responses. A bare
 *  `new Error(message)` would defeat that predicate silently. */
export function unwrapResult<T>(res: ApiResult<T>): T {
  if ("error" in res) {
    throw Object.assign(new Error(res.error.message), {
      status: res.error.status,
      code: res.error.code,
    });
  }
  return res.ok;
}

export function useUnreadCount() {
  return useQuery<UnreadCount>({
    queryKey: ["notifications", "unread_count"],
    queryFn: async () => unwrapResult(await notificationsApi.unreadCount()),
    refetchInterval: POLL_INTERVAL_MS,
    refetchOnWindowFocus: true,
  });
}

export function useNotificationList(filter: "all" | "unread") {
  return useQuery<NotificationList>({
    queryKey: ["notifications", "list", filter],
    queryFn: async () => unwrapResult(await notificationsApi.list(filter)),
  });
}

export function useMarkRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (ids: number[] | "all") => {
      unwrapResult(
        ids === "all" ? await notificationsApi.markAllRead() : await notificationsApi.markRead(ids),
      );
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["notifications"] });
    },
  });
}
