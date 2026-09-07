-- notifications: constrain `kind`, and seed task_assigned dedupe rows
-- Created 2026-09-07
--
-- Two fixes from the pre-merge review of the notifications-inbox branch.

-- 1. `kind` was left unconstrained on the theory that `PgNotificationStore`
--    was the sole write path, so application code was the only thing that
--    needed to agree on the five valid values. That is no longer true:
--    `PgTaskStore::upsert_for_doc` (crates/knot-storage/src/tasks.rs) and
--    the sweep (crates/knot-server/src/notifications_sweep.rs) both insert
--    into this table directly, bypassing the store. With three writers,
--    cheap insurance against a typo'd literal is worth having.
ALTER TABLE notifications
  ADD CONSTRAINT notifications_kind_check
  CHECK (kind IN ('mention', 'reply', 'task_assigned', 'task_due', 'doc_shared'));

-- 2. `task_assigned` is the one notification kind that re-derives from
--    existing state (doc_tasks) rather than from a write event: every
--    reindex of a document re-evaluates every task's assignee and tries to
--    emit again, relying entirely on `notifications_dedupe` to no-op for
--    an assignment it already notified about. On a deployment that already
--    has assigned, unchecked tasks sitting in `doc_tasks` from before this
--    feature shipped, the table starts empty, so the first reindex of each
--    such document (which happens on the very next edit) finds no dedupe
--    row and notifies every assignee about their entire historical
--    backlog at once.
--
--    Pre-seed already-read dedupe rows for every currently-unchecked
--    assigned task so that first reindex is a silent no-op instead of a
--    burst. `read_at = now()` means these rows carry no unread badge and
--    produce no visible burst even though they exist.
--
--    The dedupe key must be byte-identical to what
--    `knot_storage::task_assigned_dedupe_key` (crates/knot-storage/src/tasks.rs)
--    computes at runtime:
--      task_assigned:<doc_id>:<first 8 bytes of sha256(text) as lowercase hex>:<assignee>
--    `encode(substring(sha256(convert_to(text, 'UTF8')) from 1 for 8), 'hex')`
--    reproduces that hex prefix exactly, ASCII and non-ASCII text alike —
--    pinned by
--    crates/knot-storage/tests/notifications.rs::sql_and_rust_task_assigned_dedupe_keys_agree.
--    If a future change to either side breaks that parity, this seed
--    becomes silently useless (rows inserted under keys the runtime never
--    looks up again) rather than loudly wrong, so that test is load-bearing.
--
--    `data` matches the `excerpt` field name `PgTaskStore` now writes
--    (previously `text` — aligned with the rest of the inbox in the same
--    review pass) even though these rows are never rendered unread.
INSERT INTO notifications
  (workspace_id, user_id, actor_id, kind, doc_id, target_kind, target_id, dedupe_key, data, read_at)
SELECT
  t.workspace_id,
  t.assignee_user_id,
  NULL,
  'task_assigned',
  t.doc_id,
  'task',
  t.id,
  'task_assigned:' || t.doc_id || ':' ||
    encode(substring(sha256(convert_to(t.text, 'UTF8')) from 1 for 8), 'hex') || ':' ||
    t.assignee_user_id,
  jsonb_build_object('excerpt', t.text),
  now()
FROM doc_tasks t
WHERE t.assignee_user_id IS NOT NULL
  AND t.checked = false
ON CONFLICT (user_id, dedupe_key) DO NOTHING;
