import type { Notification } from "../../lib/notifications.api";

export const KIND_LABEL: Record<Notification["kind"], string> = {
  mention: "mentioned you",
  reply: "replied in your thread",
  task_assigned: "assigned you a task",
  task_due: "task is overdue",
  doc_shared: "shared a document with you",
};

/** Where a notification takes you. A comment-anchored one appends
 *  ?thread=<id> so DocPage can open the sidebar on that thread. */
export function targetPath(n: Notification): string | null {
  if (!n.doc_id) return null;
  const thread = typeof n.data.thread_id === "string" ? n.data.thread_id : null;
  return thread ? `/doc/${n.doc_id}?thread=${thread}` : `/doc/${n.doc_id}`;
}
