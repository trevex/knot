import { apiFetch, type ApiResult } from "./api";

export type Comment = {
  id: string;
  doc_id: string;
  thread_id: string;
  parent_id: string | null;
  author_id: string;
  body: string;
  position_y: string | null;        // base64-encoded Y.RelativePosition (range start)
  position_y_end: string | null;    // base64-encoded Y.RelativePosition (range end)
  anchor_text: string | null;
  created_at: string;
  updated_at: string;
  resolved_at: string | null;
  reactions: Record<string, string[]>;  // emoji → user_ids
};

export const ALLOWED_EMOJIS = ["👍", "🎉", "❤️", "🚀", "👀", "🙏"] as const;
export type AllowedEmoji = (typeof ALLOWED_EMOJIS)[number];

export const commentsApi = {
  async list(docId: string, includeResolved = false): Promise<ApiResult<Comment[]>> {
    const params = new URLSearchParams();
    if (includeResolved) params.set("include_resolved", "true");
    const qs = params.toString();
    return apiFetch<Comment[]>(
      `/api/docs/${encodeURIComponent(docId)}/comments${qs ? `?${qs}` : ""}`,
    );
  },

  async createThread(
    docId: string,
    body: string,
    positionY: string | null,
    positionYEnd: string | null,
    anchorText: string | null,
    mentions: string[] = [],
  ): Promise<ApiResult<Comment>> {
    return apiFetch<Comment>(`/api/docs/${encodeURIComponent(docId)}/comments`, {
      method: "POST",
      body: {
        body,
        position_y: positionY,
        position_y_end: positionYEnd,
        anchor_text: anchorText,
        mentions,
      },
    });
  },

  async reply(
    docId: string,
    threadId: string,
    body: string,
    mentions: string[] = [],
  ): Promise<ApiResult<Comment>> {
    return apiFetch<Comment>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(threadId)}/replies`,
      { method: "POST", body: { body, mentions } },
    );
  },

  async update(docId: string, commentId: string, body: string): Promise<ApiResult<Comment>> {
    return apiFetch<Comment>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(commentId)}`,
      { method: "PATCH", body: { body } },
    );
  },

  async remove(docId: string, commentId: string): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(commentId)}`,
      { method: "DELETE" },
    );
  },

  async resolve(docId: string, threadId: string): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(threadId)}/resolve`,
      { method: "POST" },
    );
  },

  async unresolve(docId: string, threadId: string): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(threadId)}/unresolve`,
      { method: "POST" },
    );
  },

  async addReaction(docId: string, commentId: string, emoji: AllowedEmoji): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(commentId)}/reactions`,
      { method: "POST", body: { emoji } },
    );
  },

  async removeReaction(docId: string, commentId: string, emoji: string): Promise<ApiResult<void>> {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(docId)}/comments/${encodeURIComponent(commentId)}/reactions/${encodeURIComponent(emoji)}`,
      { method: "DELETE" },
    );
  },
};
