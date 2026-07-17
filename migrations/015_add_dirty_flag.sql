-- Add a per-folder dirty flag to sync_state.
-- Set when a cache-affecting IMAP mutation (move/delete/expunge/flag change)
-- succeeds, so the hourly reconcile job knows which folders need their cache
-- pruned and flags refreshed. Cleared only on a successful reconcile.
ALTER TABLE sync_state ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
