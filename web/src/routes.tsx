import { lazy, Suspense, useEffect } from "react";
import type { ReactNode } from "react";
import { createBrowserRouter, Navigate, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";

import { RequireAuth } from "./auth/RequireAuth";
import { AppShell } from "./components/AppShell";
import { docsApi } from "./features/docs/docs.api";

const LoginPage = lazy(() => import("./features/auth/LoginPage"));
const SetupPage = lazy(() => import("./features/auth/SetupPage"));
const DocPage = lazy(() => import("./features/docs/DocPage"));
const PermissionsDialog = lazy(() => import("./features/permissions/PermissionsDialog"));
const MembersPage = lazy(() => import("./features/workspace/MembersPage"));
const SettingsPage = lazy(() => import("./features/workspace/SettingsPage"));
const PublicDoc = lazy(() => import("./features/public/PublicDoc"));
const LibraryReturn = lazy(() => import("./features/boards/LibraryReturn"));
const TasksPage = lazy(() => import("./features/tasks/TasksPage"));
const TemplatesPage = lazy(() => import("./features/docs/TemplatesPage"));
const NotificationsPage = lazy(() => import("./features/notifications/NotificationsPage"));

function Lazy({ children }: { children: ReactNode }) {
  return <Suspense fallback={<div style={{ padding: 24 }}>Loading…</div>}>{children}</Suspense>;
}

function Landing() {
  const nav = useNavigate();
  const docs = useQuery({ queryKey: ["docs"], queryFn: () => docsApi.list() });
  useEffect(() => {
    if (docs.data && "ok" in docs.data && docs.data.ok.length > 0) {
      const firstId = docs.data.ok[0]!.id;
      void nav(`/doc/${firstId}`, { replace: true });
    }
  }, [docs.data, nav]);
  return (
    <div style={{ padding: 24 }}>
      {docs.data && "ok" in docs.data && docs.data.ok.length === 0 ? (
        <>
          <h2>Welcome to knot</h2>
          <p>Create your first document from the sidebar.</p>
        </>
      ) : (
        "Loading…"
      )}
    </div>
  );
}

export const router = createBrowserRouter([
  { path: "/login", element: <Lazy><LoginPage /></Lazy> },
  { path: "/setup", element: <Lazy><SetupPage /></Lazy> },
  { path: "/p/:token", element: <Lazy><PublicDoc /></Lazy> },
  { path: "/library-return", element: <Lazy><LibraryReturn /></Lazy> },
  {
    element: <RequireAuth />,
    children: [
      {
        element: <AppShell />,
        children: [
          { index: true, element: <Landing /> },
          {
            path: "doc/:id",
            element: <Lazy><DocPage /></Lazy>,
            children: [
              { path: "permissions", element: <Lazy><PermissionsDialog /></Lazy> },
            ],
          },
          { path: "members", element: <Lazy><MembersPage /></Lazy> },
          { path: "settings", element: <Lazy><SettingsPage /></Lazy> },
          { path: "tasks", element: <Lazy><TasksPage /></Lazy> },
          { path: "templates", element: <Lazy><TemplatesPage /></Lazy> },
          { path: "notifications", element: <Lazy><NotificationsPage /></Lazy> },
        ],
      },
    ],
  },
  { path: "*", element: <Navigate to="/" replace /> },
]);
