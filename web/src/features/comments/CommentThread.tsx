import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { SmilePlus } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useSession } from "../../auth/SessionContext";
import { Avatar } from "../../components/ui/Avatar";
import { commentsApi, ALLOWED_EMOJIS, type Comment } from "../../lib/comments.api";
import { workspaceApi } from "../workspace/workspace.api";
import { useUi } from "../../stores/ui";
import { CommentComposer } from "./CommentComposer";

function relTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

// ---------------------------------------------------------------------------
// Reaction row
// ---------------------------------------------------------------------------

function ReactionRow({
  docId,
  comment,
  currentUserId,
}: {
  docId: string;
  comment: Comment;
  currentUserId: string;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [emojiPickerOpen, setEmojiPickerOpen] = useState(false);

  const addReaction = useMutation({
    mutationFn: (emoji: string) =>
      commentsApi.addReaction(docId, comment.id, emoji as (typeof ALLOWED_EMOJIS)[number]),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't add reaction"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const removeReaction = useMutation({
    mutationFn: (emoji: string) => commentsApi.removeReaction(docId, comment.id, emoji),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't remove reaction"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const existing = Object.entries(comment.reactions);

  return (
    <div className="flex flex-wrap gap-1 mt-1">
      {existing.map(([emoji, userIds]) => {
        const reacted = userIds.includes(currentUserId);
        return (
          <button
            key={emoji}
            type="button"
            data-testid={`comment-react-emoji-${comment.id}-${emoji}`}
            onClick={() => {
              if (reacted) removeReaction.mutate(emoji);
              else addReaction.mutate(emoji);
            }}
            className={`px-2 py-0.5 rounded-full text-[13px] border transition-colors ${
              reacted
                ? "border-accent bg-accent/10 text-fg"
                : "border-border bg-surface text-fg-muted hover:text-fg hover:bg-muted"
            }`}
          >
            {emoji} {userIds.length}
          </button>
        );
      })}

      <div className="relative">
        <button
          type="button"
          data-testid={`comment-react-add-${comment.id}`}
          onClick={() => setEmojiPickerOpen((o) => !o)}
          aria-label="Add reaction"
          className="inline-flex items-center justify-center px-2 h-6 rounded-full text-[13px] border border-border bg-surface text-fg-muted hover:text-fg hover:bg-muted transition-colors"
        >
          <SmilePlus size={13} aria-hidden />
        </button>
        {emojiPickerOpen && (
          <div className="absolute top-full left-0 z-10 mt-1 flex gap-1 p-1 rounded-md bg-surface border border-border shadow-lg">
            {ALLOWED_EMOJIS.map((emoji) => (
              <button
                key={emoji}
                type="button"
                onClick={() => { setEmojiPickerOpen(false); addReaction.mutate(emoji); }}
                className="text-lg p-1 rounded hover:bg-muted transition-colors"
              >
                {emoji}
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Single comment row
// ---------------------------------------------------------------------------

function CommentRow({
  docId,
  comment,
  currentUserId,
  authorName,
}: {
  docId: string;
  comment: Comment;
  currentUserId: string;
  authorName: string;
}) {
  return (
    <div data-testid={`comment-body-${comment.id}`} className="mb-3">
      <div className="flex gap-2 items-center mb-1">
        <Avatar name={authorName} seed={comment.author_id} size="sm" />
        <span className="font-semibold text-[13px] text-fg">{authorName}</span>
        <span className="text-fg-muted text-[12px]">{relTime(comment.created_at)}</span>
      </div>
      <p className="m-0 text-sm text-fg whitespace-pre-wrap pl-7">{comment.body}</p>
      <div className="pl-7">
        <ReactionRow docId={docId} comment={comment} currentUserId={currentUserId} />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------

interface Props {
  docId: string;
  threadId: string;
  root: Comment;
  replies: Comment[];
}

export function CommentThread({ docId, threadId, root, replies }: Props) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const currentUserId = sessionUser?.user_id ?? "";

  const { doc: docRole } = useEffectiveRole(docId);
  const canManage = docRole === "owner" || docRole === "editor";
  const isResolved = root.resolved_at !== null;

  const membersQuery = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
    staleTime: 60_000,
  });
  const memberMap = new Map(
    membersQuery.data && "ok" in membersQuery.data
      ? membersQuery.data.ok.map((m) => [m.user_id, m.display_name])
      : [],
  );

  function authorName(userId: string): string {
    return memberMap.get(userId) ?? "Unknown";
  }

  const resolve = useMutation({
    mutationFn: () => commentsApi.resolve(docId, threadId),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't resolve thread"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const unresolve = useMutation({
    mutationFn: () => commentsApi.unresolve(docId, threadId),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't unresolve thread"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const replyMutation = useMutation({
    mutationFn: (v: { body: string; mentions: string[] }) =>
      commentsApi.reply(docId, threadId, v.body, v.mentions),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't post reply"); return; }
      await qc.invalidateQueries({ queryKey: ["comments", docId] });
    },
  });

  const activeCommentId = useUi((s) => s.activeCommentId);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);
  const isActive = activeCommentId === threadId;
  const rowRef = useRef<HTMLDivElement | null>(null);

  // When this thread becomes active (e.g. the user clicked its highlight in
  // the editor) scroll it into view inside the sidebar.
  useEffect(() => {
    if (isActive && rowRef.current) {
      rowRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [isActive]);

  // Scroll the matching editor highlight into view when the thread is
  // clicked in the sidebar. This is a best-effort lookup — the highlight
  // span carries data-comment-id; CSS handles the visual emphasis.
  function scrollToHighlight() {
    const el = document.querySelector(`[data-comment-id="${threadId}"]`);
    if (el) el.scrollIntoView({ behavior: "smooth", block: "center" });
  }

  return (
    <div
      ref={rowRef}
      data-testid={`comment-thread-${threadId}`}
      data-active={isActive ? "true" : undefined}
      onClick={() => {
        setActiveCommentId(threadId);
        scrollToHighlight();
      }}
      className={`border-l-4 ${
        root.anchor_text && !isResolved ? "border-l-amber-500" : "border-l-transparent"
      } border-b border-border px-4 py-3 cursor-pointer transition-colors ${
        isResolved
          ? "bg-muted/40 opacity-80"
          : isActive
            ? "bg-amber-500/10 ring-1 ring-amber-500/40"
            : "bg-surface hover:bg-muted/40"
      }`}
    >
      <div className="flex justify-between items-start mb-2 gap-2">
        {root.anchor_text ? (
          <blockquote className="m-0 mb-2 px-2 py-1 border-l-[3px] border-amber-500 bg-amber-500/10 text-[12px] text-fg rounded-sm">
            {root.anchor_text}
          </blockquote>
        ) : <span />}
        {canManage && (
          isResolved ? (
            <button
              type="button"
              data-testid={`comment-unresolve-${threadId}`}
              onClick={() => unresolve.mutate()}
              disabled={unresolve.isPending}
              className="text-[12px] px-2 h-6 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors disabled:opacity-40"
            >
              Unresolve
            </button>
          ) : (
            <button
              type="button"
              data-testid={`comment-resolve-${threadId}`}
              onClick={() => resolve.mutate()}
              disabled={resolve.isPending}
              className="text-[12px] px-2 h-6 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors disabled:opacity-40"
            >
              Resolve
            </button>
          )
        )}
      </div>

      {/* Root comment */}
      <CommentRow
        docId={docId}
        comment={root}
        currentUserId={currentUserId}
        authorName={authorName(root.author_id)}
      />

      {/* Replies */}
      {replies.length > 0 && (
        <div className="border-l-2 border-border pl-3 mt-2">
          {replies.map((r) => (
            <CommentRow
              key={r.id}
              docId={docId}
              comment={r}
              currentUserId={currentUserId}
              authorName={authorName(r.author_id)}
            />
          ))}
        </div>
      )}

      {/* Reply composer — hidden when resolved */}
      {!isResolved && (
        <CommentComposer
          placeholder="Reply…"
          submitLabel="Reply"
          isPending={replyMutation.isPending}
          onSubmit={(body, mentions) => replyMutation.mutate({ body, mentions })}
          data-testid-input={`comment-composer-input-reply-${threadId}`}
          data-testid-submit={`comment-composer-submit-reply-${threadId}`}
        />
      )}
    </div>
  );
}
