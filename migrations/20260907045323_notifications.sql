-- notifications
-- Created 2026-09-07

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

-- Idempotency: every emit is ON CONFLICT DO NOTHING against this index.
CREATE UNIQUE INDEX notifications_dedupe ON notifications(user_id, dedupe_key);
-- Inbox listing, newest first, keyset-paged on id.
CREATE INDEX notifications_inbox ON notifications(user_id, id DESC);
-- The polled badge query.
CREATE INDEX notifications_unread ON notifications(user_id) WHERE read_at IS NULL;

