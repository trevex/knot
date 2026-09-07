import { type Editor } from "@tiptap/core";
import { EditorContent, useEditor } from "@tiptap/react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import * as Y from "yjs";
import { useNavigate } from "react-router-dom";

import { useQuery, useQueryClient } from "@tanstack/react-query";

import { useSession } from "../../auth/SessionContext";
import { colorFor } from "../../components/ui/Avatar";
import { blobsApi } from "../../lib/blobs.api";
import { commentsApi } from "../../lib/comments.api";
import { useUi } from "../../stores/ui";

import { encodeAnchorRange } from "../comments/anchor";
import { workspaceApi } from "../workspace/workspace.api";
import { computeAddCommentPos } from "./addCommentPos";
import { createExtensions } from "./extensions";
import {
  type HighlightedComment,
  refreshHighlights,
  setEditorRef,
} from "./CommentsHighlightExtension";
import { EditorToolbar } from "./EditorToolbar";
import { KnotProvider, type CommentChangeMsg, type ProviderStatus } from "./KnotProvider";
import { looksLikeMarkdown, markdownToHtml } from "./markdownPaste";

type Pair = { doc: Y.Doc; provider: KnotProvider };

export function KnotEditor({
  docId,
  onStatus,
  onPendingBytes,
  role,
  editMode,
}: {
  docId: string;
  onStatus: (s: ProviderStatus) => void;
  /** Polled snapshot of provider.pendingBytes(); lets the doc-page chrome
   *  surface "Saving…" vs "Saved" while we're connected. Called frequently
   *  — keep the consumer cheap. */
  onPendingBytes?: (bytes: number) => void;
  role: "owner" | "editor" | "viewer";
  editMode: boolean;
}) {
  const [pair, setPair] = useState<Pair | null>(null);

  // Own the Y.Doc + KnotProvider lifecycle inside an effect so React 18
  // StrictMode's double-mount in dev cannot leak a duplicate WebSocket.
  useEffect(() => {
    const doc = new Y.Doc();
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const provider = new KnotProvider({
      url: `${proto}//${window.location.host}/collab/doc/${docId}`,
      doc,
    });
    setPair({ doc, provider });
    onStatus(provider.status);
    const fn = (s: ProviderStatus) => onStatus(s);
    provider.on("status", fn);
    // Poll the WS buffer twice a second. Cheaper than wiring per-update
    // bookkeeping and more accurate than counting `send()` calls since
    // bufferedAmount reflects what the OS socket has actually drained.
    let lastBytes = -1;
    const pendingTimer = window.setInterval(() => {
      const bytes = provider.pendingBytes();
      if (bytes !== lastBytes) {
        lastBytes = bytes;
        onPendingBytes?.(bytes);
      }
    }, 500);
    return () => {
      window.clearInterval(pendingTimer);
      provider.off("status", fn);
      provider.destroy();
      doc.destroy();
      setPair(null);
    };
  }, [docId, onStatus, onPendingBytes]);

  if (!pair) {
    return (
      <div data-testid="editor-host" className="rounded border border-border bg-surface px-4 py-6 min-h-[240px] text-fg-muted text-sm">
        Connecting…
      </div>
    );
  }
  return <EditorBody pair={pair} role={role} docId={docId} editMode={editMode} />;
}

const IMAGE_RE = /^image\/(png|jpe?g|gif|webp)$/;
function isImageType(t: string): boolean { return IMAGE_RE.test(t); }

function EditorBody({ pair, role, docId, editMode }: { pair: Pair; role: "owner" | "editor" | "viewer"; docId: string; editMode: boolean }) {
  const canEdit = role !== "viewer" && editMode;
  const navigate = useNavigate();
  const qc = useQueryClient();
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const userColor = useMemo(() => colorFor(sessionUser?.user_id ?? "anon"), [sessionUser]);
  const notify = useUi((s) => s.notify);
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);
  const setPendingAnchor = useUi((s) => s.setPendingAnchor);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);
  const activeCommentId = useUi((s) => s.activeCommentId);
  const editorRef = useRef<Editor | null>(null);
  // ⌘⇧V / Ctrl+Shift+V is the conventional "paste without formatting". The
  // paste event carries no modifier state of its own, so latch it from the
  // keydown that triggers the paste.
  const plainPasteRef = useRef(false);

  // Fetch all comments (including resolved) for the highlight overlay.
  // We deliberately use a different cache key from the sidebar's "active
  // threads only" list so toggling Show Resolved doesn't refetch this.
  const commentsForHighlight = useQuery({
    queryKey: ["comments", docId, "all-for-highlight"],
    queryFn: () => commentsApi.list(docId, true),
    staleTime: 0,
  });
  const highlightComments: HighlightedComment[] = useMemo(() => {
    if (!commentsForHighlight.data || "error" in commentsForHighlight.data) return [];
    return commentsForHighlight.data.ok
      .filter((c) => c.parent_id === null && c.position_y && c.position_y_end && !c.resolved_at)
      .map((c) => ({
        id: c.id,
        thread_id: c.thread_id,
        position_y: c.position_y as string,
        position_y_end: c.position_y_end as string,
      }));
  }, [commentsForHighlight.data]);

  const [presence, setPresence] = useState<Array<{ name: string; color: string }>>([]);

  // Floating "Add comment" button state
  const [addCommentPos, setAddCommentPos] = useState<{ top: number; left: number } | null>(null);
  const [selectionRange, setSelectionRange] = useState<{ from: number; to: number } | null>(null);

  useEffect(() => {
    const { provider } = pair;
    const update = () => {
      const states = Array.from(provider.awareness.getStates().values()) as Array<
        { user?: { name?: string; color?: string } }
      >;
      setPresence(
        states
          .filter((s) => s.user?.name)
          .map((s) => ({ name: s.user!.name!, color: s.user!.color ?? "#666" })),
      );
    };
    provider.awareness.on("change", update);
    update();
    return () => { provider.awareness.off("change", update); };
  }, [pair]);

  // Invalidate the comments query when a peer pushes a MSG_COMMENTS frame,
  // causing the comment sidebar to refetch automatically.
  useEffect(() => {
    const { provider } = pair;
    const onComments = (_msg: CommentChangeMsg) => {
      void qc.invalidateQueries({ queryKey: ["comments", docId] });
    };
    provider.on("comments", onComments);
    return () => { provider.off("comments", onComments); };
  }, [pair, docId, qc]);

  const uploadAndInsert = useCallback(async (files: File[]) => {
    for (const f of files) {
      const r = await blobsApi.upload(docId, f);
      if ("error" in r) {
        notify(
          "error",
          r.error.code === "blob.too_large" ? "File too large (10 MB cap)."
            : r.error.code === "blob.blocked_type" ? "File type not allowed."
            : r.error.code === "acl.no_grant" ? "You don't have permission to upload here."
            : "Upload failed.",
        );
        continue;
      }
      const blob = r.ok;
      if (isImageType(blob.content_type)) {
        editorRef.current?.chain().focus().setImage({ src: blob.url }).run();
      } else {
        editorRef.current?.chain().focus().insertContent({
          type: "attachment",
          attrs: {
            url: blob.url,
            name: blob.original_name ?? f.name,
            size: blob.byte_size,
            contentType: blob.content_type,
          },
        }).run();
      }
    }
  }, [docId, notify]);

  const canComment = role === "owner" || role === "editor";

  const editor = useEditor(
    {
      extensions: createExtensions({
        doc: pair.doc,
        awareness: pair.provider.awareness,
        user: { name: sessionUser?.display_name ?? "Anonymous", color: userColor },
        onSelectComment: (commentId) => {
          setActiveCommentId(commentId);
          openCommentSidebar();
        },
        navigate,
        fetchMembers: async () => {
          const r = await workspaceApi.listMembers();
          if ("error" in r) return [];
          return r.ok.map((m) => ({
            user_id: m.user_id,
            display_name: m.display_name,
            email: m.email,
          }));
        },
      }),
      editable: canEdit,
      editorProps: {
        handleDOMEvents: {
          keydown: (_view, event) => {
            if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "v") {
              plainPasteRef.current = event.shiftKey;
            }
            return false;
          },
        },
        handleDrop(_view, event, _slice, _moved) {
          const files = Array.from((event as DragEvent).dataTransfer?.files ?? []);
          if (files.length === 0) return false;
          event.preventDefault();
          void uploadAndInsert(files);
          return true;
        },
        handlePaste(_view, event) {
          const clipboard = (event as ClipboardEvent).clipboardData;
          const files = Array.from(clipboard?.files ?? []);
          if (files.length > 0) {
            event.preventDefault();
            void uploadAndInsert(files);
            return true;
          }
          // Consume the latch on every paste, so a plain-text paste never
          // leaks its opt-out into the next one.
          const plainOnly = plainPasteRef.current;
          plainPasteRef.current = false;
          if (plainOnly) return false;
          // A rich-text source already carries structure; ProseMirror's own
          // clipboard parser handles it better than a Markdown round-trip.
          if (clipboard?.types.includes("text/html")) return false;
          const ed = editorRef.current;
          // Inside a code block or a code mark, a paste must stay verbatim.
          if (!ed || ed.isActive("code_block") || ed.isActive("code")) return false;
          const text = clipboard?.getData("text/plain") ?? "";
          if (!looksLikeMarkdown(text)) return false;
          event.preventDefault();
          ed.chain().focus().insertContent(markdownToHtml(text)).run();
          notify("info", "Pasted as Markdown");
          return true;
        },
      },
      onSelectionUpdate: ({ editor: ed }) => {
        if (!canComment) return;
        const { from, to } = ed.state.selection;
        if (from === to) {
          setAddCommentPos(null);
          setSelectionRange(null);
          return;
        }
        const coords = ed.view.coordsAtPos(from);
        const host = ed.view.dom.parentElement ?? ed.view.dom;
        const hostRect = host.getBoundingClientRect();
        // The toolbar is sticky, so its live rect is the only honest way to
        // tell whether the button would land under it — a host-relative
        // offset says nothing once the host has scrolled beneath it.
        const toolbar = document.querySelector("[data-testid='editor-toolbar']");
        setAddCommentPos(
          computeAddCommentPos({
            caret: { top: coords.top, bottom: coords.bottom, left: coords.left },
            host: { top: hostRect.top, left: hostRect.left, width: hostRect.width },
            toolbarBottom: toolbar ? toolbar.getBoundingClientRect().bottom : null,
          }),
        );
        setSelectionRange({ from, to });
      },
    },
    [pair, sessionUser?.user_id, role, userColor, uploadAndInsert, canComment],
  );

  // Keep ref in sync so uploadAndInsert (stable callback) can reach the
  // latest editor instance. useLayoutEffect ensures the ref is updated
  // before any DOM event handler that might call uploadAndInsert during
  // the commit phase fires (drag/paste).
  useLayoutEffect(() => {
    editorRef.current = editor ?? null;
  }, [editor]);

  // Reflect editMode toggles into the live editor without re-creating it
  // (re-creating would tear down the Y binding, awareness, and history).
  useEffect(() => {
    if (!editor) return;
    if (editor.isEditable !== canEdit) editor.setEditable(canEdit);
  }, [editor, canEdit]);

  // Register the editor with the highlight extension's ref holder. The
  // plugin needs an editor reference to decode Y.RelativePositions; this
  // closes the loop without a circular import.
  useEffect(() => {
    setEditorRef(editor ?? null);
    return () => { setEditorRef(null); };
  }, [editor]);

  // Push the latest comments + activeCommentId into the highlight extension's
  // storage, then dispatch a no-op transaction so the plugin re-decorates.
  useEffect(() => {
    // Navigating from one document to another runs this effect twice against
    // the OUTGOING editor before the replacement exists — React commits the
    // new deps first. Tiptap 3 empties extensionStorage on destroy, so the
    // lookup below returns undefined and assigning to it throws, taking the
    // whole route down with an error boundary. In v2 the same write landed on
    // a dead object and did nothing, which is why this was never guarded.
    //
    // e2e/flows/search.spec.ts is the regression test: navigating via the
    // command palette reproduces it. Clicking a sidebar link does not — the
    // timing differs — so do not "simplify" that spec into a plain link click.
    if (!editor || editor.isDestroyed) return;
    const storage = editor.extensionStorage.commentsHighlight;
    storage.comments = highlightComments;
    storage.activeCommentId = activeCommentId;
    storage.doc = pair.doc;
    refreshHighlights(editor);
  }, [editor, highlightComments, activeCommentId, pair.doc]);

  // Re-decorate when the Yjs doc fires an update — the local doc change
  // already triggers a PM tr (docChanged), but a peer update arrives via
  // applyUpdate which may not always carry through. Belt-and-braces.
  useEffect(() => {
    if (!editor) return;
    const onUpdate = () => { refreshHighlights(editor); };
    pair.doc.on("update", onUpdate);
    return () => { pair.doc.off("update", onUpdate); };
  }, [editor, pair.doc]);

  function handleAddComment() {
    if (!editor || !selectionRange) return;
    const { from, to } = selectionRange;
    let anchorText = editor.state.doc.textBetween(from, to, " ").slice(0, 120);
    // Atom blocks (excalidraw_board, etc.) have no text content. Fall back
    // to a node-specific label so the comment doesn't display an empty
    // quote.
    if (!anchorText.trim()) {
      editor.state.doc.nodesBetween(from, to, (node) => {
        if (anchorText.trim()) return false;
        if (node.type.name === "excalidraw_board") {
          const label = (node.attrs.label as string | null)?.trim();
          anchorText = label && label.length > 0 ? `Diagram: ${label}` : "Diagram";
          return false;
        }
        return true;
      });
    }
    const { start, end } = encodeAnchorRange(editor, pair.doc, from, to);
    setPendingAnchor({
      positionY: start ?? "",
      positionYEnd: end ?? "",
      anchorText,
    });
    openCommentSidebar();
    setAddCommentPos(null);
  }

  return (
    <>
      <PresenceBar presence={presence} />

      {canEdit && (
        <EditorToolbar
          editor={editor}
          docId={docId}
          onUploadFiles={(files) => { void uploadAndInsert(files); }}
        />
      )}
      <div
        data-testid="editor-host"
        className="relative"
      >
        {/* Floating "Add comment" button */}
        {canComment && addCommentPos && (
          <button
            type="button"
            data-testid="add-comment-float"
            onClick={handleAddComment}
            className="absolute z-20 bg-accent text-accent-fg border-none rounded px-2 py-0.5 text-[12px] font-medium cursor-pointer shadow-md hover:opacity-90 transition-opacity"
            style={{ top: addCommentPos.top, left: addCommentPos.left }}
          >
            Add comment
          </button>
        )}
        <EditorContent editor={editor} />
      </div>
    </>
  );
}

type Peer = { name: string; color: string };

function PresenceBar({ presence }: { presence: Peer[] }) {
  if (presence.length === 0) {
    return <div data-testid="presence-bar" className="h-7 mb-2" aria-hidden />;
  }
  const visible = presence.slice(0, 5);
  const overflow = presence.length - visible.length;
  return (
    <div
      data-testid="presence-bar"
      className="mb-2 flex items-center"
      aria-label={`${presence.length} people editing`}
    >
      <div className="flex -space-x-1.5">
        {visible.map((p, i) => (
          <span
            key={i}
            title={p.name}
            aria-label={p.name}
            className="inline-flex items-center justify-center h-7 w-7 rounded-full text-white text-[12px] font-semibold ring-2 ring-bg shadow-sm select-none"
            style={{ background: p.color }}
          >
            {p.name.slice(0, 1).toUpperCase()}
          </span>
        ))}
        {overflow > 0 && (
          <span
            title={`${overflow} more`}
            className="inline-flex items-center justify-center h-7 min-w-7 px-1.5 rounded-full bg-muted text-fg-muted text-[11px] font-semibold ring-2 ring-bg shadow-sm"
          >
            +{overflow}
          </span>
        )}
      </div>
      <span className="ml-3 text-[12px] text-fg-muted">
        {presence.length === 1 ? "1 person" : `${presence.length} people`} here
      </span>
    </div>
  );
}
