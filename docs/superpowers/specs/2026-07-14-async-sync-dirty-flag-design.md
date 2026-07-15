# Async sync_emails, dirty flag, and hourly reconcile — Design

Date: 2026-07-14
Status: Approved
Branch: feat/managesieve (implementation may use its own branch)

## Problem

1. The MCP `sync_emails` tool runs the entire IMAP sync synchronously inside the
   HTTP request (`src/dashboard/api/handlers.rs:3594` awaits
   `SyncService::sync_folder` / `sync_all_folders`). A full INBOX sync measured
   at 14m50s, while the `rustymail-mcp-stdio` proxy caps every request at 30s
   (`MCP_TIMEOUT`, `src/bin/mcp_stdio.rs:22`). Every long sync therefore fails
   client-side with a misleading "Failed to connect to backend" error while the
   server keeps syncing.
2. MCP message mutations (move, delete, expunge, mark read/unread) go through
   `EmailService` (`src/dashboard/services/email.rs:424-639`) and touch only the
   IMAP server, never the SQLite cache. The periodic incremental sync
   (`start_sync_process_spawner`, `src/main.rs:378`, every
   `SYNC_INTERVAL_SECONDS`, default 300) only fetches new UIDs and never prunes
   deleted/moved rows or refreshes flags — dead-row pruning happens only on a
   full sync, which effectively never runs after the first. The cache drifts
   permanently after mutations.

## Goals

1. `sync_emails` returns immediately; the sync runs in the background.
2. A per-folder dirty flag records that a cache-affecting mutation happened.
3. An hourly job reconciles dirty folders automatically.

Non-goals: replacing the 5-minute incremental sync (kept as-is, per user
decision); updating the cache inline at mutation time; IMAP IDLE.

## Design

### 1. Data model — dirty flag

New migration `migrations/015_add_dirty_flag.sql`:

```sql
ALTER TABLE sync_state ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
```

Granularity is per folder per account (`sync_state` is keyed by `folder_id`;
`folders.account_id` scopes it to an account).

New `CacheService` helpers next to `update_sync_state` (`cache.rs:867`):

- `mark_folder_dirty(folder_name, account_email)` — upsert, because a
  never-synced folder may have no `sync_state` row yet (use
  `get_or_create_folder_for_account` + `INSERT ... ON CONFLICT(folder_id) DO
  UPDATE SET dirty = 1`).
- `clear_folder_dirty(folder_name, account_email)`.

### 2. Setting the flag — one choke point

Each mutating `EmailService` method calls `mark_folder_dirty` on the affected
folder(s) immediately after the IMAP operation succeeds (never before — a
failed IMAP op changes nothing):

| Operation | Folders marked dirty |
|---|---|
| `atomic_move_message`, `atomic_batch_move` | source and destination |
| `mark_as_deleted`, `delete_messages`, `undelete_messages`, `expunge` | the folder |
| `mark_as_read`, `mark_as_unread` | the folder (cached flags go stale) |

`cache_service` is `Option<Arc<CacheService>>` on `EmailService`; when `None`,
skip silently. The live instance is built with `.with_cache(...)`
(`services/mod.rs:208`). The REST `delete_email` path (`handlers.rs:5137`)
bypasses `EmailService` but already deletes its cache rows inline
(`cache_service.delete_emails_by_uids`, `handlers.rs:5176`); it stays as-is.

### 3. Async `sync_emails` — spawn the sync binary

The MCP arm at `handlers.rs:3594` stops awaiting the in-process `SyncService`
and instead spawns `rustymail-sync --account <id> [--folder <f>] --reconcile`,
returning immediately:

- `{"status": "started", "message": "Sync started for ..."}` on spawn, or
- `{"status": "already_running"}` when the lock file is held (the binary exits
  with code 2, detected via `try_wait`), or
- an explicit error if the spawn itself fails.

The locate-binary + spawn + `try_wait`/reaper logic currently duplicated in the
REST `trigger_email_sync` handler (`handlers.rs:4693-4764`) and
`start_sync_process_spawner` (`main.rs:397-414`) is extracted into one shared
helper — new small module `src/dashboard/services/sync_spawner.rs` — used by
the REST handler, the MCP handler, and both `main.rs` spawners. The in-process
`SyncService` sync path becomes unreachable from MCP but is not deleted in this
change.

Rationale for subprocess over `tokio::spawn`: sync was deliberately moved out
of process so the OS reclaims sync memory (see `src/bin/sync.rs` header
comment and commits 66c4809, c71e3f3); the MCP path converges on the same
engine.

### 4. Reconcile — new sync-binary capability

Reconcile of one folder = `SEARCH ALL` for the live UID set → prune dead cache
rows (existing `prune_dead_rows`) → fetch FLAGS for remaining cached UIDs in
chunks and update cached flags → clear the folder's dirty flag. No body
re-download; the expensive `--force` full re-download is unrelated and
unchanged.

Two new `rustymail-sync` flags (reconcile logic lives in a new library module,
`src/sync_reconcile.rs` — flat in `src/` like `forensic.rs` — with free
functions taking `&SqlitePool` and the IMAP client, so the 791-line
`src/bin/sync.rs` barely grows and the logic is unit-testable):

- `--reconcile` — after the normal incremental pass, reconcile each *target*
  folder whose dirty flag is set. Used by MCP `sync_emails`, so
  "delete messages, then sync" leaves the cache consistent.
- `--reconcile-dirty` — query `sync_state` joined to `folders` for all dirty
  folders across all accounts, group by account, connect once per account,
  reconcile each dirty folder (plus incremental new-UID fetch for those
  folders), exit 0 with "no dirty folders" when none. Used by the hourly job.

The existing 5-minute spawner keeps invoking the binary with no flags — pure
incremental, unchanged cost.

### 5. Hourly trigger

A second spawner in `main.rs` next to `start_sync_process_spawner`, same
shape: `tokio::time::interval` reading `DIRTY_SYNC_INTERVAL_SECONDS` (default
3600; documented in `.env.example`), each tick spawning
`rustymail-sync --reconcile-dirty` via the shared spawn helper. It does not
query the DB itself — the "is anything dirty" check lives in exactly one
place, the binary. The existing lock file serializes it against the 5-minute
syncs and manual syncs.

### 6. Status visibility — `get_sync_status` MCP tool

New read-only MCP tool (registered in the tool list and dispatch in
`handlers.rs`, backed by the same `sync_state` queries as REST
`GET /api/sync/status`, `handlers.rs:4807`):

- Params: `account_id` (required), `folder` (optional).
- With `folder`: that folder's row — `status` (Idle/Syncing/Error),
  `emails_synced`, `emails_total`, `last_incremental_sync`, `last_full_sync`,
  `error_message`, `dirty`; `"never_synced"` when no row.
- Without `folder`: the same fields for every folder of the account.

Agents call `sync_emails`, then poll `get_sync_status` until `Idle`.

### 7. Error handling

- Reconcile failure mid-folder: dirty flag stays set (cleared only on that
  folder's success); `sync_state.status = Error` with message; next hourly
  tick retries.
- Spawn failure: MCP tool returns an explicit error.
- Lock contention: `already_running` response; dirty flag persists, nothing
  lost.
- `mark_folder_dirty` DB failure: log a warning, do not fail the IMAP
  operation (the mutation itself succeeded — same posture as existing
  cache-write failures during sync).

### 8. Testing

- `CacheService::mark_folder_dirty` / `clear_folder_dirty`: unit tests against
  an in-memory SQLite pool (existing pattern in `cache.rs` tests), including
  the no-`sync_state`-row upsert case.
- Reconcile module: seed cache rows + dirty flag, feed a fake live-UID set,
  assert dead rows pruned, flags updated, dirty cleared; failure path asserts
  dirty survives.
- `get_sync_status`: handler test with seeded `sync_state` rows, with and
  without `folder`, plus the `never_synced` case.
- End-to-end manual verification before commit: delete a message via MCP →
  `dirty = 1` → run `rustymail-sync --reconcile-dirty` → row pruned,
  `dirty = 0`, `get_sync_status` reflects it; `sync_emails` returns in under a
  second and `already_running` surfaces when the lock is held.

## Expected effects

- `sync_emails` responds instantly instead of timing out at 30s.
- Deleted/moved messages disappear from the cache within an hour
  automatically, or immediately after an explicit `sync_emails`.
- Read/unread flags heal on the same schedule.
- The 5-minute incremental sync and the memory-reclaim process model are
  untouched.

## Configuration added

| Variable | Default | Purpose |
|---|---|---|
| `DIRTY_SYNC_INTERVAL_SECONDS` | 3600 | Interval for the dirty-folder reconcile spawner |
