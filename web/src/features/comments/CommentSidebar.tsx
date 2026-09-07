import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useState } from "react";

import { IconButton } from "../../components/ui/IconButton";
import { commentsApi, type Comment } from "../../lib/comments.api";
import { useUi } from "../../stores/ui";
import { CommentComposer } from "./CommentComposer";
import { CommentThread } from "./CommentThread";

type ThreadGroup = {
  threadId: string;
  root: Comment;
  replies: Comment[];
  resolvedAt: string | null;
};

function groupByThread(comments: Comment[]): ThreadGroup[] {
  const roots = new Map<string, Comment>();
  const repliesMap = new Map<string, Comment[]>();

  for (const c of comments) {
    if (c.parent_id === null) {
      roots.set(c.thread_id, c);
    } else {
      const list = repliesMap.get(c.thread_id) ?? [];
      list.push(c);
      repliesMap.set(c.thread_id, list);
    }
  }

  const groups: ThreadGroup[] = [];
  for (const [threadId, root] of roots) {
    const replies = (repliesMap.get(threadId) ?? []).slice().sort(
      (a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime(),
    );
    groups.push({ threadId, root, replies, resolvedAt: root.resolved_at });
  }

  // Active threads first, then resolved; within each bucket sort by creation time
  groups.sort((a, b) => {
    const aResolved = a.resolvedAt !== null;
    const bResolved = b.resolvedAt !== null;
    if (aResolved !== bResolved) return aResolved ? 1 : -1;
    return new Date(a.root.created_at).getTime() - new Date(b.root.created_at).getTime();
  });

  return groups;
}

export function CommentSidebar({ docId }: { docId: string }) {
  const [showResolved, setShowResolved] = useState(false);
  const closeCommentSidebar = useUi((s) => s.closeCommentSidebar);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);
  const pendingAnchor = useUi((s) => s.pendingAnchor);
  const clearPendingAnchor = useUi((s) => s.clearPendingAnchor);
  const notify = useUi((s) => s.notify);
  const qc = useQueryClient();

  const list = useQuery({
    queryKey: ["comments", docId, showResolved],
    queryFn: () => commentsApi.list(docId, showResolved),
  });

  const createThread = useMutation({
    mutationFn: (v: { body: string; mentions: string[] }) =>
      commentsApi.createThread(
        docId,
        v.body,
        pendingAnchor?.positionY ?? null,
        pendingAnchor?.positionYEnd ?? null,
        pendingAnchor?.anchorText ?? null,
        v.mentions,
      ),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't post comment"); return; }
      clearPendingAnchor();
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const comments: Comment[] = list.data && "ok" in list.data ? list.data.ok : [];
  const groups = groupByThread(comments);

  return (
    <div
      role="dialog"
      data-testid="comment-sidebar"
      className="fixed right-0 top-0 h-dvh w-[400px] max-w-full z-40 bg-surface border-l border-border shadow-xl flex flex-col"
    >
      <header className="flex items-center gap-2 px-4 py-3 border-b border-border">
        <h2 className="m-0 flex-1 text-base font-semibold text-fg">Comments</h2>
        <label className="text-[13px] text-fg-muted flex items-center gap-1.5 cursor-pointer select-none">
          <input
            type="checkbox"
            data-testid="comment-show-resolved"
            checked={showResolved}
            onChange={(e) => setShowResolved(e.target.checked)}
            className="accent-accent"
          />
          Show resolved
        </label>
        <IconButton
          data-testid="comment-sidebar-close"
          label="Close"
          size="sm"
          onClick={() => { setActiveCommentId(null); closeCommentSidebar(); }}
        >
          <X size={14} aria-hidden />
        </IconButton>
      </header>

      {pendingAnchor && (
        <div className="border-b border-border bg-accent/5">
          <div className="px-4 pt-2 text-[12px] text-fg-muted">
            New comment on: <em>&ldquo;{pendingAnchor.anchorText}&rdquo;</em>
          </div>
          <CommentComposer
            placeholder="Start a new thread…"
            submitLabel="Comment"
            isPending={createThread.isPending}
            onSubmit={(body, mentions) => createThread.mutate({ body, mentions })}
            data-testid-input="comment-composer-input-new"
            data-testid-submit="comment-composer-submit-new"
          />
        </div>
      )}

      <div className="flex-1 overflow-y-auto">
        {list.isLoading && (
          <p className="px-4 py-3 text-fg-muted text-sm">Loading…</p>
        )}
        {!list.isLoading && groups.length === 0 && !pendingAnchor && (
          <p className="px-4 py-3 text-fg-muted text-sm">No comments yet.</p>
        )}
        {groups.map((g) => (
          <CommentThread
            key={g.threadId}
            docId={docId}
            threadId={g.threadId}
            root={g.root}
            replies={g.replies}
          />
        ))}
      </div>
    </div>
  );
}
