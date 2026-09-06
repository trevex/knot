# Notifications and inbox — design

**Date:** 2026-09-06
**Status:** Approved (brainstorm)
**Origin:** the deferred "Plan 19.5 — Mention push bridge" in
`docs/superpowers/research/2026-06-03-plan19-outcome.md:72-81`, widened from a
toast to an inbox.

## Context

knot has no way to tell you that something happened. The one event that was
meant to reach a user — an `@mention` in a comment — is wired at both ends and
connected in the middle to nothing.

`broadcast_mentions` (`crates/knot-server/src/routes/api/comments.rs:143`)
extracts handles from a comment body, resolves them to workspace member ids, and
fires `pg_notify('comment_mentions', …)` with a JSON payload. No process in the
codebase ever runs `LISTEN` for that channel. `bus_pg` subscribes only to
`doc:<id>` and `presence:<id>` (`crates/knot-crdt/src/bus_pg.rs:208-211`), and
the two `PgListener` background tasks that exist — `comments_listener.rs` and the
ACL invalidation listener — take `doc_comments` and `acl_invalidate`.

On the other end, `KnotProvider` reserves `MSG_MENTION` (type 4) and maintains a
`mention` listener list (`web/src/features/editor/KnotProvider.ts:7-8,42,195`)
that cannot fire. The file has said so in a comment since 2026-06-03.

So mentioning a colleague today does nothing they will ever see. The same is true
of every other event worth knowing about: a reply in a thread you started, a
checklist item assigned to you, a task going overdue, a page shared with you.

This spec covers all five, in-app only, with the persistence shaped so an email
transport can be added later without a migration.

## Why not just finish the mention bridge

The obvious minimal fix is to add a `comment_mentions` listener mirroring
`comments_listener.rs`, route the payload into the doc room, and emit
`MSG_MENTION` as the June plan intended. That delivers a toast to users who
already have that document open — which is the one case where they would have
seen the comment anyway.

An inbox has to reach you when you have no document open at all. The doc
WebSocket cannot do that: rooms are keyed by `doc_id`, and a user sitting on
`/tasks` or the landing page is in no room. So the transport is per-user, not
per-doc, and the reserved `MSG_MENTION` plumbing is deleted rather than
completed.

## Design

### 1. Schema

One additive migration. No backfill: mentions that happened before this ships
stay unnotified.

```sql
CREATE TABLE notifications (
  id            bigserial PRIMARY KEY,
  workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  actor_id      uuid NULL REFERENCES users(id) ON DELETE SET NULL,
  kind          text NOT NULL,
  doc_id        uuid NULL REFERENCES documents(id) ON DELETE CASCADE,
  target_kind   text NOT NULL,
  target_id     text NOT NULL,
  dedupe_key    text NOT NULL,
  data          jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  read_at       timestamptz NULL,
  emailed_at    timestamptz NULL
);

CREATE UNIQUE INDEX notifications_dedupe ON notifications(user_id, dedupe_key);
CREATE INDEX notifications_inbox  ON notifications(user_id, id DESC);
CREATE INDEX notifications_unread ON notifications(user_id) WHERE read_at IS NULL;
```

- `user_id` is the **recipient**, `actor_id` whoever caused it (NULL for
  system-generated events, i.e. `task_due`).
- `kind` ∈ `mention | reply | task_assigned | task_due | doc_shared`.
- `target_id` is **TEXT**, not UUID, because `doc_tasks.id` is
  `"<doc_id>:<item_index>"` (`migrations/20260604120000_doc_tasks.sql`).
- `data` carries the denormalised excerpt, doc title at emit time, and
  `thread_id` where applicable, so rendering the inbox needs no joins beyond
  actor and current doc title.
- `dedupe_key` + its unique index is the idempotency mechanism. Every emit is
  `ON CONFLICT (user_id, dedupe_key) DO NOTHING`, which makes concurrent emits
  from multiple replicas safe without coordination.
- `emailed_at` is the entire "email-ready" affordance: because emit writes a row
  rather than pushing, the table *is* an outbox. A future mailer polls
  `WHERE emailed_at IS NULL AND created_at < now() - interval '2 minutes'` (the
  delay collapses a burst into one message) and per-user preferences arrive as
  their own table. This table does not change.

### 2. Emit

A single `knot_storage::notifications::emit(&mut tx, Notification)` — takes an
executor so callers already inside a transaction stay atomic with it. Emits
where `actor_id = user_id` are dropped in `emit` itself, not at each call site.

Five sources:

| kind | Where | `dedupe_key` |
| --- | --- | --- |
| `mention` | `routes/api/comments.rs`, replacing `broadcast_mentions` | `mention:<comment_id>` |
| `reply` | same request path | `reply:<comment_id>` |
| `doc_shared` | grant create in `routes/api/grants.rs` | `share:<doc_id>:<grantee>` |
| `task_assigned` | `knot_storage::tasks` reindex upsert | `task_assigned:<doc_id>:<sha256(text)[..16]>:<assignee>` |
| `task_due` | periodic sweep (§4) | `task_due:<task_id>:<due date>` |

**Reply recipients** are the distinct authors of non-deleted comments in the
thread, minus the actor, minus anyone already receiving a `mention` row for the
same comment. Being mentioned in a reply to your own thread produces one
notification, not two.

**The `task_assigned` hazard.** `doc_tasks.id` is `"<doc_id>:<item_index>"`, and
`upsert_for_doc` (`crates/knot-storage/src/tasks.rs:95-140`) deletes rows that
fell out of the new id set and re-inserts the rest on *every* reindex — which
runs on every document edit. Keying the notification off the row id or off "row
inserted with an assignee" would re-notify every assignee every time anyone
reordered a list. Hence the content-addressed key: reordering a list does not
re-notify, and editing a task's text does. That trade is deliberate — an edited
task is arguably a new task, and the alternative (tracking identity across
reorders) is a much larger change to a table whose id scheme documents itself as
intentionally unstable.

### 3. Comment mention identity

Document mentions already carry exact identity: `MentionExtension.ts:33` inserts
a link to `knot://user/<uuid>`, and `knot_markdown::tasks` lifts the assignee
back out of that sentinel.

Comment mentions do not. `MentionPicker.tsx:52-56` inserts plain
`@${display_name}` text, and the server re-guesses the recipient with
`Regex::new(r"(?:^|\s)@(\w+)")` matched against lowercased display names
(`comments.rs:31,133`). `\w` is Unicode-aware in the `regex` crate under the
default features this workspace takes (`Cargo.toml:97`), so `@Jörg` resolves
fine. The real defect is the space: `@Christian Hüning` captures `Christian`,
matches no member's full display name, and silently notifies nobody. Any member
whose display name has a space in it — most of them — is unmentionable. Today
that fails invisibly; with an inbox it becomes "why does my colleague never get
pinged".

The editor's `knot://user/<uuid>` sentinel is **not** the fix here. Comment
bodies render as plain text — `CommentThread.tsx:132` drops `comment.body` into
a `whitespace-pre-wrap` paragraph — so a markdown link would show every reader
its raw source. Rendering comment bodies as markdown would work, but it pulls
`marked` plus HTML sanitisation into the comment path, and an XSS surface is a
steep price for a mention chip.

Instead the client sends what it already knows. `MentionPicker` records the
`user_id` of each member picked; `POST/PATCH /api/comments` gains an optional
`mentions: [uuid]` field. The server validates each id is a member of the
comment's workspace and uses that list as the recipients. The body stays plain
`@Christian Hüning` text and renders exactly as it does today.

When `mentions` is absent — comments written before this ships, and any
scripted client — the display-name regex remains as the fallback path. Editing a
comment replaces its mention list, and `dedupe_key` is per comment, so adding
someone in an edit notifies them once and re-notifies nobody.

### 4. Sweep

One `tokio::interval` task, 15 minutes, spawned per replica alongside the
existing listener tasks in `main.rs`. Two jobs:

1. **Overdue tasks** — `doc_tasks WHERE due_at < now() AND NOT checked AND
   assignee_user_id IS NOT NULL`, emitting `task_due` with a date-stamped dedupe
   key so an overdue task notifies once per day, not once per sweep.
2. **Retention** — delete read rows older than 90 days, and any row older than
   180 days.

Every replica runs both. The unique index plus `ON CONFLICT DO NOTHING` makes
concurrent sweeps idempotent, so no leader election, no advisory locks, and no
new operational component. The cost is a redundant indexed scan per replica per
15 minutes.

### 5. HTTP API

All routes are session-authenticated and scoped to `ctx.user_id`. Ownership needs
no join — a row belongs to exactly one recipient — but document access is a
separate question, re-checked on read below.

- `GET /api/notifications?filter=unread|all&limit=50&cursor=<id>` — keyset paging
  on `id DESC`, joined to actor display name and current doc title.
- `GET /api/notifications/unread_count` — `{"count": n, "capped": bool}`. The
  count runs `SELECT count(*) FROM (SELECT 1 … LIMIT 100) t` so an inbox nobody
  has read for a year cannot turn the polled endpoint into a sequential scan;
  the UI renders `99+`.
- `POST /api/notifications/read` — body `{"ids": […]}` or `{"all": true}`.

**Stale access.** A notification can outlive your access to the document it
points at. The list handler re-checks `effective_role` for rows carrying a
`doc_id` and filters them out of the response, the same defensive re-check
`routes/api/search.rs:1-5` performs on FTS candidates. Revoking a grant does not
have to rewrite anyone's inbox, and a revoked user's stale rows are unreachable
rather than 403-producing.

### 6. Transport: polling

`useUnreadCount()` is a TanStack Query with `refetchInterval: 30_000` and
`refetchOnWindowFocus: true`. Returning to the tab refreshes immediately, which
covers most of the perceived latency; the worst case for a user staring at an
open tab is 30 seconds.

This deliberately adds no new subsystem. The alternative considered was a
per-user SSE stream (`GET /api/notifications/stream`) with one `PgListener` per
replica fanning out in-process — the exact shape of `comments_listener.rs`. It
is the better end state, but it costs a long-lived connection per tab, plus
heartbeats, a connection cap, and multi-tab handling, and it reads the same table
through the same endpoints. It can replace polling behind an unchanged API and
schema if 30 seconds proves too slow in real use.

### 7. UI

- An **Inbox** link with an unread badge in
  `web/src/features/workspace/WorkspaceHeader.tsx:40`, beside the existing Tasks
  link. Badge hidden at zero.
- A **dropdown** showing the last 10: actor, kind, doc title, excerpt, relative
  time. Clicking marks that row read and navigates to its target.
- A **`/notifications` route**, lazy-loaded like every other route in
  `routes.tsx:49-70`, with an all/unread filter and "Mark all read".

### 8. Thread deep-linking

Nothing in `DocPage` reads a thread from the URL today, so clicking a mention
would land you at the top of the document with no indication of where the
comment is. `/doc/:id?thread=<uuid>` opens the comments panel and scrolls that
thread into view. Small, but load-bearing: a notification you cannot act on is
not worth sending.

### 9. Removals

- `broadcast_mentions` and the `comment_mentions` channel
  (`comments.rs:141-188`) — replaced by `emit`.
- `MSG_MENTION`, the `mention` listener list, and `MentionMsg` in
  `KnotProvider.ts` — dead since June, and this design routes around the doc
  WebSocket entirely.

## Observability

One counter, `knot_notifications_emitted_total{kind}`, registered in `knot-obs`
beside the existing metrics. The sweep logs its emit and prune counts at debug.

## Testing

**`crates/knot-storage/tests/notifications.rs`**

- Concurrent `emit` with the same `dedupe_key` produces exactly one row.
- Self-notification (`actor_id = user_id`) writes nothing.
- **Reorder characterisation:** reindex a doc whose two assigned tasks have
  swapped positions; assert no new rows. This is §2's hazard as an executable
  assertion, and it is the test most likely to catch a future change to the task
  id scheme.
- Editing a task's text does emit — the documented other half of that trade.
- Sweep: an overdue task emits once, a second sweep in the same day no-ops, the
  next day emits again.
- Retention prunes read rows past 90 days and everything past 180.
- Deleting a document cascades its notifications away.

**`crates/knot-server/tests/notifications_integration.rs`**

- Mention writes a row for the mentioned user and none for the author.
- Reply writes rows for thread participants minus the actor; a user both
  mentioned and a participant gets exactly one row.
- A comment carrying `mentions: [<uuid>]` notifies that user even though their
  display name contains a space; a comment sent without the field still resolves
  `@Alice` through the regex fallback.
- An id in `mentions` for a non-member is rejected, not notified.
- Granting access emits `doc_shared`.
- The list endpoint omits rows whose doc access was revoked.
- `unread_count` reports `capped: true` past 100.
- `POST /read` with ids, and with `{all: true}`.

**Vitest** — badge hidden at zero and capped at `99+`; dropdown rendering;
optimistic mark-read.

**`e2e/flows/notifications.spec.ts`** — two browser contexts. Alice comments
`@Bob` on a doc Bob can read; Bob's badge appears, the dropdown lists it,
clicking navigates to the doc with the thread open and scrolled, and the badge
clears. The spec drives the refresh with a window focus event rather than
waiting out the poll interval. A second case mentions a member whose display
name contains a space, which fails on `main` and passes here.

## Sequencing

1. Migration, `emit`, dedupe, storage tests.
2. Mention + reply + `doc_shared` call sites; integration tests.
3. `task_assigned` (content-addressed key) and the sweep.
4. HTTP endpoints.
5. Comment mention identity (§3).
6. Thread deep-linking (§8).
7. UI: badge, dropdown, `/notifications`.
8. e2e.
9. Delete the dead plumbing (§9).

Steps 1–4 are shippable without any UI; steps 5 and 6 are independently useful.

## Non-goals

- **Email.** No SMTP, no templates, no preferences. `emailed_at` reserves the
  seam; nothing writes it.
- **SSE / live push.** §6 — deliberately deferred, and cheap to add later.
- **Watching a page.** "Notify me of any edit to this doc" is a different
  subscription model and a much noisier one.
- **Per-user notification preferences.** Everything in §2 notifies. Preferences
  are worth building once email exists and the noise is real.
- **Push notifications / service worker.** The web manifest added in 0.4.0 makes
  this possible later; it is not this.
- **Cross-workspace inbox.** knot is single-workspace today; `workspace_id` is
  on the table so this does not become a migration later.
