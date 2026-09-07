import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Download, Eye, FileCode, History, LayoutTemplate, MessageSquare, Pencil, Share2 } from "lucide-react";
import { lazy, Suspense, useEffect, useState, type CSSProperties } from "react";
import { Link, Outlet, useNavigate, useParams, useSearchParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { StatusDot, SyncStatus, type ConnStatus } from "../../components/StatusDot";
import { IconButton } from "../../components/ui/IconButton";
import { useUi } from "../../stores/ui";

import { CommentSidebar } from "../comments/CommentSidebar";
import { MarkdownView } from "../editor/MarkdownView";
import { Breadcrumb } from "./Breadcrumb";
import { docsApi } from "./docs.api";
import { editModeKey } from "./editMode";
import { HistoryDrawer } from "./HistoryDrawer";
import { DocWidthToggle } from "./DocWidthToggle";
import { ImportMarkdownButton } from "./ImportMarkdownButton";

const KnotEditor = lazy(() =>
  import("../editor/KnotEditor").then((m) => ({ default: m.KnotEditor })),
);

export function DocTitle({
  id,
  initialTitle,
  editable,
}: {
  id: string;
  initialTitle: string;
  editable: boolean;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [title, setTitle] = useState(initialTitle);

  const rename = useMutation({
    mutationFn: async (next: string) => docsApi.patch(id, { title: next }),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify("error", "Couldn't rename");
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      await qc.invalidateQueries({ queryKey: ["doc", id] });
    },
  });

  return (
    <input
      data-testid="doc-title"
      value={title}
      readOnly={!editable}
      onChange={(e) => setTitle(e.target.value)}
      onBlur={() => { if (editable && title !== initialTitle) rename.mutate(title); }}
      placeholder="Untitled"
      className={`w-full border-none bg-transparent text-[30px] font-bold text-fg placeholder:text-fg-muted/60 focus:outline-none focus:ring-0 px-0 ${
        editable ? "" : "cursor-default"
      }`}
    />
  );
}

export default function DocPage() {
  const { id } = useParams<{ id: string }>();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { doc: effRole } = useEffectiveRole(id);
  const notify = useUi((s) => s.notify);
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [pendingBytes, setPendingBytes] = useState(0);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [mdView, setMdView] = useState(false);
  // View mode by default — viewers are always read-only anyway; editors and
  // owners get a per-tab toggle (persisted in window.sessionStorage by doc id) so a
  // page refresh keeps the chosen mode but a fresh tab starts safe.
  const editModeKeyOrNull = id ? editModeKey(id) : null;
  const [editMode, setEditMode] = useState<boolean>(() => {
    // localStorage override for automation: any non-empty value defaults
    // every doc to edit mode. Used by the e2e suite to avoid threading a
    // `toggle-edit-mode` click through every spec; harmless in production
    // because nothing in the app writes this key.
    try {
      if (window.localStorage.getItem("knot.editMode.defaultOn") === "1") return true;
    } catch { /* localStorage unavailable */ }
    if (!editModeKeyOrNull) return false;
    return window.sessionStorage.getItem(editModeKeyOrNull) === "1";
  });
  useEffect(() => {
    if (!editModeKeyOrNull) return;
    window.sessionStorage.setItem(editModeKeyOrNull, editMode ? "1" : "0");
  }, [editMode, editModeKeyOrNull]);
  // ⌘E / Ctrl+E toggles edit mode for editor+. Viewers stay read-only.
  useEffect(() => {
    if (effRole !== "owner" && effRole !== "editor") return;
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "e") {
        e.preventDefault();
        setEditMode((v) => !v);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [effRole]);
  // ⌘⇧F / Ctrl+Shift+F toggles the layout width. Collision-free: the ⌘E
  // handler above bails on e.shiftKey, and no browser binds ⌘⇧F.
  const toggleDocWidth = useUi((s) => s.toggleDocWidth);
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "f") {
        e.preventDefault();
        toggleDocWidth();
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggleDocWidth]);
  const commentSidebarOpen = useUi((s) => s.commentSidebarOpen);
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);
  const setActiveCommentId = useUi((s) => s.setActiveCommentId);

  // Arriving from a notification: /doc/:id?thread=<uuid>. The sidebar's
  // existing active-thread machinery (CommentThread.tsx) does the scrolling
  // and the focus ring — this just opens the sidebar and points it at the
  // thread. Keyed on the URL param alone so it fires once per navigation and
  // doesn't fight a later in-sidebar thread click.
  const [searchParams] = useSearchParams();
  const threadParam = searchParams.get("thread");
  useEffect(() => {
    if (!threadParam) return;
    openCommentSidebar();
    setActiveCommentId(threadParam);
  }, [threadParam, openCommentSidebar, setActiveCommentId]);

  const doc = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id!),
    enabled: Boolean(id),
  });

  useEffect(() => {
    if (status === "unauthorised") {
      notify("error", "You no longer have access to this document.");
      void nav("/", { replace: true });
    }
  }, [status, notify, nav]);

  if (!id) return null;
  if (doc.isLoading) {
    return <div className="doc-shell text-fg-muted">Loading…</div>;
  }
  if (!doc.data || "error" in doc.data) {
    return <div className="doc-shell text-fg-muted">Document not found.</div>;
  }

  const meta = doc.data.ok;

  // Named rather than an inline `async` handler: `onClick` is typed to
  // return void, so handing it a promise leaves rejections unhandled.
  // Callers below fire it with `void`, the same idiom the editor uses for
  // its own async work.
  async function toggleTemplate() {
    const next = !meta.is_template;
    const r = await docsApi.setTemplate(id!, next);
    if ("error" in r) {
      notify("error", next ? "Couldn't save as template" : "Couldn't unmark");
      return;
    }
    notify("info", next ? "Saved as template" : "Removed from templates");
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["templates"] });
    await doc.refetch();
  }

  return (
    <section
      data-testid="doc-page"
      className="doc-shell"
      // Drives the comment-rail inset in layout.css (>=1280 only), so the
      // rail reserves space instead of covering live text.
      style={{ "--knot-rail-w": commentSidebarOpen ? "400px" : "0px" } as CSSProperties}
    >
      <div className="measure">
        <Breadcrumb items={[{ title: "Documents" }, { title: meta.title }]} />
      </div>
      {/* Wraps rather than overflows. The action row is 412px intrinsic on a
          phone (the width toggle is hidden there) against a 327px content box,
          and `shrink-0` with no wrapping used to force the last two buttons
          off-screen. The title's min-width now breaks the row instead, and the
          action row wraps internally into whatever width it is given. On the
          desktop column it still fits one line — 200 + 452 + 12 <= 712 — so
          that layout is unchanged. */}
      <div className="measure mt-3 flex flex-wrap items-start gap-3">
        <div className="flex-1 min-w-[200px]">
          <DocTitle key={id} id={id} initialTitle={meta.title}
                    editable={effRole !== "viewer" && editMode} />
        </div>
        <div className="flex flex-wrap items-center gap-1 pt-2">
          <SyncStatus sync={{ status, pendingBytes }} />
          {/* Keep the bare StatusDot mounted (invisible) so existing tests
              targeting `data-testid="status-dot"` still find it; SyncStatus
              is the user-facing affordance now. */}
          <span className="sr-only"><StatusDot status={status} /></span>
          {effRole === "owner" && (
            <Link
              to="permissions"
              data-testid="open-permissions"
              aria-label="Share"
              title="Share & permissions"
              className="inline-flex items-center gap-1.5 h-9 px-3 rounded text-[13px] font-medium text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              <Share2 size={16} aria-hidden />
              <span>Share</span>
            </Link>
          )}
          <DocWidthToggle />
          {(effRole === "owner" || effRole === "editor") && (
            <IconButton
              data-testid="toggle-edit-mode"
              label={editMode ? "Stop editing (⌘E)" : "Edit (⌘E)"}
              active={editMode}
              onClick={() => setEditMode((v) => !v)}
            >
              {editMode ? <Eye size={16} aria-hidden /> : <Pencil size={16} aria-hidden />}
            </IconButton>
          )}
          {(effRole === "owner" || effRole === "editor") && (
            <IconButton
              data-testid="open-history"
              label="History"
              onClick={() => setHistoryOpen(true)}
            >
              <History size={16} aria-hidden />
            </IconButton>
          )}
          <IconButton
            data-testid="toggle-markdown"
            label={mdView ? "Show editor" : "Show markdown"}
            active={mdView}
            onClick={() => setMdView((v) => !v)}
          >
            <FileCode size={16} aria-hidden />
          </IconButton>
          {(effRole === "owner" || effRole === "editor") && (
            <ImportMarkdownButton docId={id} docTitle={meta.title} />
          )}
          <IconButton
            data-testid="doc-export"
            label="Export doc (shift-click for subtree)"
            onClick={(e) => {
              // Default: just this doc. Shift-click pulls the whole
              // subtree — a power-user shortcut without cluttering the
              // header with a separate button.
              const descendants = e.shiftKey;
              const a = document.createElement("a");
              a.href = `/api/docs/${id}/export?descendants=${descendants ? "true" : "false"}`;
              a.download = `${meta.title || "doc"}.zip`;
              a.click();
            }}
          >
            <Download size={16} aria-hidden />
          </IconButton>
          {effRole === "owner" && (
            <IconButton
              data-testid="toggle-template"
              label={meta.is_template ? "Remove from templates" : "Save as template"}
              active={meta.is_template}
              onClick={() => { void toggleTemplate(); }}
            >
              <LayoutTemplate size={16} aria-hidden />
            </IconButton>
          )}
          <IconButton
            data-testid="open-comments"
            label="Comments"
            onClick={openCommentSidebar}
          >
            <MessageSquare size={16} aria-hidden />
          </IconButton>
        </div>
      </div>
      <div className="mt-6">
        {mdView ? (
          <MarkdownView docId={id} />
        ) : (
          <Suspense fallback={<p className="measure text-fg-muted">Loading editor…</p>}>
            <KnotEditor
              docId={id}
              onStatus={setStatus}
              onPendingBytes={setPendingBytes}
              role={meta.effective_role}
              editMode={editMode}
            />
          </Suspense>
        )}
      </div>
      <Outlet />
      {historyOpen && id && (
        <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />
      )}
      {commentSidebarOpen && id && <CommentSidebar docId={id} />}
    </section>
  );
}
