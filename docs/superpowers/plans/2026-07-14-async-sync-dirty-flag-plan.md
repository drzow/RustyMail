# Async sync_emails, dirty flag, and hourly reconcile — Implementation Plan

Date: 2026-07-14
Spec: docs/superpowers/specs/2026-07-14-async-sync-dirty-flag-design.md (Approved — the contract)
Branch: feat/managesieve

This plan is a numbered sequence of small, independently buildable/testable steps.
Each step names exact files/functions, the concrete shape of the change, the tests
to write FIRST, and the verification command. Commit points are marked; per repo
rule, build + tests must pass before every commit.

---

## Open issues (found while verifying spec references)

### OI-1 — EmailService mutators have no account context (blocking; affects §2)

The spec §2 says each mutating `EmailService` method calls
`mark_folder_dirty(folder_name, account_email)`. But the mutators do **not**
receive an account:

- `atomic_move_message`, `atomic_batch_move`, `mark_as_read`, `mark_as_unread`,
  `mark_as_deleted`, `delete_messages`, `undelete_messages`, `expunge`
  (`src/dashboard/services/email.rs:424-643`) take only `folder`/`uids` and open
  the IMAP session with `imap_factory.create_session()` — the no-arg factory that
  uses the **default `.env` account** (`src/imap/mod.rs:66`, "credentials from .env").
- The MCP dispatch arms that call them (`handlers.rs:2078-2437`) never resolve
  `account_id` at all — they read only `uid`/`uids`/`folder` from params. So the
  account the mutation actually hit is always the default `.env` account.

`mark_folder_dirty` must scope the `sync_state` row via
`get_or_create_folder_for_account(folder_name, account_id)`, so it needs the
account email.

**Recommended resolution (used by this plan):** add one private best-effort helper
on `EmailService` that resolves the default account and marks the folder dirty:

```rust
/// Best-effort: mark a folder dirty on the default account's cache.
/// Silent no-op when cache/account service is absent; logs a warning on DB error
/// (the IMAP mutation already succeeded — never fail it for a cache-write miss).
async fn mark_folder_dirty(&self, folder: &str) {
    let (Some(cache), Some(accts)) = (self.cache_service.as_ref(), self.account_service.as_ref())
        else { return; };
    let account_email = match accts.lock().await.get_default_account().await {
        Ok(Some(a)) => a.email_address,
        Ok(None) => { warn!("mark_folder_dirty: no default account; skipping {}", folder); return; }
        Err(e) => { warn!("mark_folder_dirty: default account lookup failed: {}", e); return; }
    };
    if let Err(e) = cache.mark_folder_dirty(folder, &account_email).await {
        warn!("Failed to mark folder '{}' dirty: {}", folder, e);
    }
}
```

This changes **no** mutator signatures and **no** MCP arms — smallest possible
change, and it targets exactly the account the mutation ran against (the default
`.env` account = the account `create_session()` used). `get_default_account`
already exists (`account.rs:695`, returns `Result<Option<Account>>`) and is the
same call the REST `get_sync_status` handler uses (`handlers.rs:4817`).

`delete_messages_for_account` (`email.rs:547`) already receives an explicit
`account_id`; it marks dirty with that account directly (no default lookup). It has
no current callers but is a cache-affecting mutation path, so per CLAUDE.md rule 11
("fix all instances") it gets the hook too.

Caveat recorded for the reviewer: in a multi-account deployment where the DB
default account differs from the `.env` credentials `create_session()` binds, the
dirty flag would be attributed to the default account. That mismatch is pre-existing
(the mutators already ignore `account_id`) and out of scope here; the hourly
reconcile still heals whichever account is actually dirty.

### OI-2 — Reconcile cannot reuse `prune_dead_rows` as-is (affects §4)

Spec §4 puts reconcile in a new library module `src/sync_reconcile.rs` using "free
functions taking `&SqlitePool` and the IMAP client" and says it reuses the existing
`prune_dead_rows`. But `prune_dead_rows` (and its helper `get_or_create_folder_id`)
are **binary-private** functions in `src/bin/sync.rs:420` — a library module cannot
call them. There is a *separate* `CacheService::prune_dead_rows` method, but the
binary works on a raw `&SqlitePool`, not a `CacheService` (which also owns a memory
cache), so pulling in `CacheService` in the binary is the wrong abstraction.

Also: there is **no** pool-based flag updater today — flag writes go through
`CacheService::update_email_flags` (`cache.rs`, a method). Reconcile needs a
pool-based equivalent.

**Recommended resolution (used by this plan):** move `prune_dead_rows` and
`get_or_create_folder_id` out of `src/bin/sync.rs` into `src/sync_reconcile.rs` as
`pub` free functions, and have the binary call them via the crate-name path
`rustymail::sync_reconcile::...` (the binary is a separate crate — same pattern as
the existing `rustymail::forensic::` call at `sync.rs:503`; `crate::` there would
resolve to the binary crate and fail to compile). Add the other pool-based free
functions reconcile needs (`update_cached_flags`, `list_dirty_folders`,
`clear_dirty`) plus a **pure DB core `reconcile_cache`** (prune → flags → clear-last)
that the thin IMAP wrapper `reconcile_folder` delegates to — so the prune/flags/
dirty logic is unit-testable without IMAP (spec §8's mandated unit test). This keeps
one definition of each (no duplication), shrinks `src/bin/sync.rs`, and satisfies the
spec's stated intent. Full signatures and tests are in Step 5.

### OI-3 — `get_sync_status` must be registered in TWO tool lists (affects §6)

There are two MCP tool-definition functions, not one:
`get_mcp_tools_jsonrpc_format()` (`handlers.rs:102`, JSON-RPC shape) and
`list_mcp_tools()` (`handlers.rs:1023`, HTTP-list shape). `sync_emails` appears in
both (lines 709 and 1309). `get_sync_status` must be added to **both**, plus a
dispatch arm in `execute_mcp_tool_inner` (`handlers.rs:1464`, fallthrough `_` at
3871).

### OI-4 — Minor line drift in spec references (non-blocking)

Verified actuals (use these, not the spec's approximate lines):
- MCP `sync_emails` arm: `handlers.rs:3594` ✓ (confirmed).
- REST spawn/reap to extract: `handlers.rs:4692-4771` (spec said 4693-4764).
- Periodic spawner spawn/reap: `main.rs:406-414`, started at `main.rs:231` ✓.
- Migrations are applied automatically at CacheService init via
  `sqlx::migrate!("./migrations")` (`cache.rs:177`). Migration 015 is picked up
  there at startup — **no code change needed to run it.**
- REST `delete_email` path stays as-is (it already deletes its own cache rows);
  confirmed out of scope.

---

## Configuration added (documented in `.env.example`)

| Variable | Default | Purpose |
|---|---|---|
| `DIRTY_SYNC_INTERVAL_SECONDS` | 3600 | Interval for the dirty-folder reconcile spawner (§5) |

`SYNC_INTERVAL_SECONDS` (existing, default 300) is unchanged.

---

## Steps

### Step 1 — Migration 015: add `dirty` column

- **Goal:** persist a per-folder dirty flag in `sync_state`.
- **Files:** new `migrations/015_add_dirty_flag.sql`; `.env.example` (add
  `DIRTY_SYNC_INTERVAL_SECONDS=3600` with a comment).
- **Change:**
  ```sql
  ALTER TABLE sync_state ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
  ```
  No Rust change — `sqlx::migrate!("./migrations")` (`cache.rs:177`) applies it at
  startup and in the in-memory test pools.
- **Tests FIRST:** add a `cache.rs` test module case that builds an in-memory
  `CacheService` (existing test pattern), then `SELECT dirty FROM sync_state`
  succeeds (column exists, defaults to 0). Name: `migration_015_adds_dirty_column`.
- **Verify:** `cargo test -p rustymail migration_015_adds_dirty_column`
- **Commit:** "feat(sync): migration 015 adds sync_state.dirty flag + env var"

### Step 2 — CacheService dirty helpers

- **Goal:** set/clear the dirty flag with upsert (row may not exist yet).
- **Files:** `src/dashboard/services/cache.rs`, next to `update_sync_state`
  (`cache.rs:867`).
- **Change:** two methods modeled on `update_sync_state`'s upsert:
  ```rust
  pub async fn mark_folder_dirty(&self, folder_name: &str, account_id: &str) -> Result<(), CacheError> {
      let folder = self.get_or_create_folder_for_account(folder_name, account_id).await?;
      let pool = self.db_pool.as_ref().ok_or(CacheError::NotInitialized)?;
      sqlx::query(
          "INSERT INTO sync_state (folder_id, dirty) VALUES (?, 1)
           ON CONFLICT(folder_id) DO UPDATE SET dirty = 1, updated_at = CURRENT_TIMESTAMP"
      ).bind(folder.id).execute(pool).await?;
      Ok(())
  }

  pub async fn clear_folder_dirty(&self, folder_name: &str, account_id: &str) -> Result<(), CacheError> {
      // get_or_create so a caller clearing a never-synced folder is a harmless no-op-ish upsert to 0
      let folder = self.get_or_create_folder_for_account(folder_name, account_id).await?;
      let pool = self.db_pool.as_ref().ok_or(CacheError::NotInitialized)?;
      sqlx::query(
          "INSERT INTO sync_state (folder_id, dirty) VALUES (?, 0)
           ON CONFLICT(folder_id) DO UPDATE SET dirty = 0, updated_at = CURRENT_TIMESTAMP"
      ).bind(folder.id).execute(pool).await?;
      Ok(())
  }
  ```
  (The `INSERT ... VALUES (?, 1)` relies on `sync_state`'s other NOT NULL columns
  having defaults — confirm against `migrations/001_create_schema.sql:124` +
  `010_add_sync_progress.sql`; if `sync_status` lacks a default, include it in the
  INSERT column list as `'idle'`, matching `update_sync_state`.)
- **Tests FIRST** (in-memory pool, existing `cache.rs` test pattern):
  - `mark_folder_dirty_sets_flag` — mark, then `get`/`SELECT dirty` == 1.
  - `mark_folder_dirty_upserts_when_no_sync_state_row` — folder cached but no
    `sync_state` row yet; mark creates the row with dirty = 1 (spec §1 explicit case).
  - `clear_folder_dirty_resets_flag` — mark then clear → dirty == 0.
- **Verify:** `cargo test -p rustymail dirty`
- **Commit:** "feat(cache): mark_folder_dirty / clear_folder_dirty helpers"

### Step 3 — EmailService sets the dirty flag (the choke point, §2)

- **Goal:** every cache-affecting mutation marks the affected folder(s) dirty after
  IMAP success.
- **Files:** `src/dashboard/services/email.rs`.
- **Change:**
  1. Add the private `mark_folder_dirty(&self, folder: &str)` best-effort helper from
     OI-1.
  2. Call it after the IMAP op succeeds (never before) per the spec §2 table:
     - `atomic_move_message`: `self.mark_folder_dirty(from_folder).await;
       self.mark_folder_dirty(to_folder).await;` (after line 441 success log).
     - `atomic_batch_move`: same, source + destination (after line 463).
     - `mark_as_read` / `mark_as_unread` / `mark_as_deleted` / `undelete_messages`:
       `self.mark_folder_dirty(folder).await;` after their success logs.
     - `expunge`: `self.mark_folder_dirty(folder).await;` after line 641.
     - `delete_messages`: already delegates to `mark_as_deleted` + `expunge`, so it
       inherits the marks — **do not** add a third (avoid redundant work / matches
       "one logical mark per folder").
     - `delete_messages_for_account`: after line 603 success,
       `if let Some(cache) = self.cache_service.as_ref() { let _ = cache.mark_folder_dirty(folder, &account.email_address).await; }`
       (uses the explicit account it already loaded).
- **Confirm before committing (finding 6, per OI-1):** the helper attributes the
  dirty flag to `get_default_account()` because the mutators run against the `.env`
  account via `create_session()`. The developer must verify, for this deployment,
  that the `.env` IMAP account and the DB default account are the same account, and
  record that confirmation in the commit message (e.g. "verified .env account ==
  get_default_account() for this deployment"). If they can ever differ, escalate to
  the architect before proceeding — the dirty flag would land on the wrong account.
- **Tests FIRST:** unit test that an `EmailService` built with `cache_service = None`
  does not panic when the helper is invoked (the `None` silent-skip path). Full
  IMAP-driven mutation coverage is exercised by the Step 10 end-to-end test (no IMAP
  mock exists in-tree). Name: `mark_folder_dirty_noop_without_cache`.
- **Verify:** `cargo test -p rustymail mark_folder_dirty_noop_without_cache && cargo build`
- **Commit:** "feat(email): mark affected folders dirty after mutations"

### Step 4 — Extract the spawn/reap helper (`sync_spawner.rs`)

- **Goal:** one shared helper for locating + spawning `rustymail-sync` and reaping
  the child, replacing the copies in the REST handler and both `main.rs` spawners.
- **Files:** new `src/dashboard/services/sync_spawner.rs` (register in
  `src/dashboard/services/mod.rs`); edit `handlers.rs:4692-4771` and
  `main.rs:396-419`.
- **Change:** model exactly on the existing REST logic (`handlers.rs:4692-4771`):
  ```rust
  /// Locate the rustymail-sync binary across build/deploy layouts.
  pub fn locate_sync_binary() -> &'static str { /* the 4-way ./target/... fallback */ }

  pub enum SpawnOutcome { Started { pid: u32 }, AlreadyRunning, Completed }

  /// Spawn rustymail-sync with `args`, wait 100ms, classify via try_wait,
  /// and detach a spawn_blocking reaper for the still-running case.
  /// Exit code 2 => AlreadyRunning; success => Completed; else Err.
  pub fn spawn_sync(args: &[String]) -> std::io::Result<SpawnOutcome> { ... }
  ```
  - REST `trigger_email_sync` builds `args` (`--account`, `--folder`, `--force`) and
    maps `SpawnOutcome` to its existing JSON responses (in_progress / completed /
    syncing) — behavior unchanged.
  - `start_sync_process_spawner` (`main.rs`) calls `spawn_sync(&[])` (no flags) and
    ignores the outcome beyond logging — behavior unchanged.
  - Keep the exact binary-search order the REST path uses (it includes the
    `./target/debug/...` fallback that `main.rs` currently omits; unifying on the
    superset is a strict improvement, note it in the commit).
- **Tests FIRST:** `locate_sync_binary_returns_path` (asserts it returns one of the
  known candidates / the PATH fallback — pure, no process spawn). Spawn behavior is
  covered end-to-end in Step 10.
- **Verify:** `cargo test -p rustymail locate_sync_binary && cargo build`
- **Commit:** "refactor(sync): extract shared sync_spawner (spawn + reap + locate)"

### Step 5 — Reconcile library module (`src/sync_reconcile.rs`)

- **Goal:** pool-based reconcile logic, unit-testable, reused by the binary.
- **Files:** new `src/sync_reconcile.rs` (declare `pub mod sync_reconcile;` in
  `src/lib.rs`); edit `src/bin/sync.rs` to call the moved functions.
- **Change:**
  1. **Move** `prune_dead_rows` (`sync.rs:420`) and `get_or_create_folder_id`
     (definition `sync.rs:757`) from `src/bin/sync.rs` into `sync_reconcile.rs` as
     `pub` free fns. No logic change to either.
  2. **Update every call site.** `src/bin/sync.rs` is a *separate binary crate*, so
     it must reference the moved fns with the crate-name path
     `rustymail::sync_reconcile::...` (matching the existing
     `rustymail::forensic::create_forensic_archive` call at `sync.rs:503`). `crate::`
     would resolve to the binary crate and fail to compile; `crate::` is correct only
     inside `src/sync_reconcile.rs` itself. Call sites to update (finding 4 —
     enumerated so none are missed):
     - `prune_dead_rows`: `sync.rs:348` and `sync.rs:402`.
     - `get_or_create_folder_id`: `sync.rs:558`, `586`, `617`, `648`, and `426`
       (the last is inside `prune_dead_rows`, which moves with it, so that reference
       becomes an intra-module `get_or_create_folder_id(...)` call in
       `sync_reconcile.rs`).
     `pub mod sync_reconcile;` in `src/lib.rs` stays (flat module pattern, same as
     `pub mod forensic;`).
  3. Add pool-based free fns:
     ```rust
     /// Serialize flags EXACTLY like CacheService::update_email_flags
     /// (cache.rs:509-511): BTreeSet dedup, then serde_json::to_string — because
     /// emails.flags is a JSON-array TEXT column and readers expect that shape.
     /// UPDATE emails SET flags=?, updated_at=CURRENT_TIMESTAMP WHERE folder_id=? AND uid=?
     /// (no memory-cache eviction — the binary has no CacheService handle).
     pub async fn update_cached_flags(pool: &SqlitePool, folder_id: i64, uid: u32, flags: &[String]) -> Result<(), sqlx::Error>;

     /// All dirty folders across all accounts. `folders.account_id` IS the account
     /// email address (schema uses email as the accounts PK — no accounts.id column;
     /// migrations/001_create_schema.sql:8,61), so no accounts JOIN is needed.
     /// Returns (account_email, folder_name):
     /// SELECT f.account_id, f.name FROM sync_state s
     ///   JOIN folders f ON f.id = s.folder_id
     /// WHERE s.dirty = 1
     pub async fn list_dirty_folders(pool: &SqlitePool) -> Result<Vec<(String, String)>, sqlx::Error>;

     /// UPDATE sync_state SET dirty=0, updated_at=CURRENT_TIMESTAMP WHERE folder_id=?
     pub async fn clear_dirty(pool: &SqlitePool, folder_id: i64) -> Result<(), sqlx::Error>;

     /// PURE DB reconcile (no IMAP) — the unit-testable core (finding 3 / spec §8):
     /// 1. prune_dead_rows against `live_uids`
     /// 2. update_cached_flags for each (uid, flags) in `flag_updates`
     /// 3. clear_dirty(folder_id) — LAST, and only if 1 & 2 both succeeded.
     /// Returns Err (WITHOUT clearing dirty) if any step fails, so a failed
     /// reconcile leaves the folder dirty for the next tick (spec §7).
     pub async fn reconcile_cache(
         pool: &SqlitePool,
         folder_id: i64,
         account_email: &str,
         folder_name: &str,
         live_uids: &[u32],
         flag_updates: &[(u32, Vec<String>)],
     ) -> Result<(), sqlx::Error>;

     /// IMAP wrapper: select + SEARCH ALL + fetch_flags (chunks of 500), then
     /// delegate to reconcile_cache. Any IMAP error returns Err before
     /// reconcile_cache runs, so dirty is untouched (spec §7).
     pub async fn reconcile_folder(
         pool: &SqlitePool,
         client: &ImapClient<AsyncImapSessionWrapper>,
         account_email: &str,
         folder_name: &str,
     ) -> Result<(), Box<dyn std::error::Error>>;
     ```
     `reconcile_folder`: resolve `folder_id` via `get_or_create_folder_id`;
     `client.select_folder(folder)`; `client.search_emails("ALL")` (`client.rs:148`)
     → `live_uids`; read remaining cached uids
     (`SELECT uid FROM emails WHERE folder_id = ?`); `client.fetch_flags(chunk)`
     (`client.rs:156`, returns `Vec<(u32, Vec<String>)>`) in chunks of 500 →
     collect `flag_updates`; then a single
     `reconcile_cache(pool, folder_id, ..., &live_uids, &flag_updates)`. The IMAP
     loop mirrors `SyncService::sync_flags_for_folder` (`sync.rs:510-550`), the
     proven flag-refresh loop; the DB mutation is isolated in `reconcile_cache`.
- **Tests FIRST** (in-memory pool, no IMAP — this is why the split exists):
  - `reconcile_cache_prunes_updates_and_clears_dirty` (happy path, spec §8): seed
    an account/folder, cached rows (some dead), `dirty = 1`; call `reconcile_cache`
    with a live-UID subset + flag updates; assert dead rows pruned, flags rewritten,
    `dirty = 0`.
  - `reconcile_cache_leaves_dirty_on_failure` (failure path, spec §8): force a SQL
    error by dropping/renaming the `emails` table in the test pool before the call
    (deterministic failure injection); assert `reconcile_cache` returns `Err` and
    `dirty` is still `1` (clear never reached).
  - `list_dirty_folders_returns_marked` — seed accounts/folders/sync_state with
    mixed dirty values; assert only dirty ones returned with correct email + name.
  - `clear_dirty_resets_row`.
  - `update_cached_flags_writes_json` — seed an email row, update flags, read back
    the JSON and assert BTreeSet-deduped/serialized shape (finding 5).
  - `prune_dead_rows_removes_absent_uids` — move/adapt any existing coverage.
  - `reconcile_folder` (the IMAP wrapper) is exercised against a live account in
    Step 10 (no in-tree IMAP mock); its DB effects are already unit-covered via
    `reconcile_cache`.
- **Verify:** `cargo test -p rustymail sync_reconcile && cargo build --bin rustymail-sync`
- **Commit:** "feat(sync): sync_reconcile module (prune+flags+dirty), reused by binary"

### Step 6 — Sync binary `--reconcile` / `--reconcile-dirty` flags

- **Goal:** expose reconcile from `rustymail-sync`.
- **Files:** `src/bin/sync.rs` (the `Cli` struct at line 36, `main` at 131,
  `sync_account` at 242).
- **Change:**
  1. Add two `clap` bools to `Cli`:
     ```rust
     /// After incremental sync, reconcile dirty target folders for the account.
     #[arg(long)] reconcile: bool,
     /// Reconcile every dirty folder across all accounts, then exit.
     #[arg(long)] reconcile_dirty: bool,
     ```
  2. `--reconcile-dirty` (checked early in `main`, before the normal account loop):
     `list_dirty_folders(&pool)`; if empty, log "no dirty folders" and `return Ok(())`;
     else group by account, look up each account row (reuse the existing accounts
     query), connect once per account (reuse the `sync_account` connection block),
     and for each dirty folder: run the normal incremental `sync_folder(...)` (new-UID
     fetch, spec §4 "plus incremental new-UID fetch") then
     `reconcile_folder(...)`. Logout per account.
  3. `--reconcile` (account-scoped): after `sync_account`'s normal folder loop, for
     each synced folder whose dirty flag is set (`SELECT dirty` per folder), call
     `reconcile_folder(...)` on the already-open client before logout. Pass the
     `reconcile` bool into `sync_account`.
  4. `--reconcile` requires `--account` (mirror the existing `--folder requires
     --account` validation at `sync.rs:140-143`); `--reconcile-dirty` takes neither.
  5. All calls into the reconcile module from `sync.rs` use the crate-name path
     `rustymail::sync_reconcile::{list_dirty_folders, reconcile_folder, clear_dirty}`
     (separate binary crate — same reason as Step 5 finding 1).
- **Tests FIRST:** `cargo build --bin rustymail-sync` compiles the new flags;
  arg-validation (`--reconcile` without `--account` exits non-zero) is asserted in
  Step 10. (No unit test spawns the full binary here.)
- **Verify:** `cargo build --bin rustymail-sync`
- **Commit:** "feat(sync): --reconcile and --reconcile-dirty flags"

### Step 7 — MCP `sync_emails` becomes async spawn (§3)

- **Goal:** `sync_emails` returns immediately by spawning the binary with
  `--reconcile`.
- **Files:** `src/dashboard/api/handlers.rs:3594-3643`.
- **Change:** replace the in-process `sync_service.sync_folder/sync_all_folders`
  awaits with `sync_spawner::spawn_sync`:
  - args = `["--account", <account_id>, "--reconcile"]`, plus `["--folder", <f>]`
    when `folder` is present.
  - Map `SpawnOutcome`:
    - `Started{..}` → `{"success": true, "data": {"status": "started", "message": ...}}`
    - `AlreadyRunning` → `{"success": true, "data": {"status": "already_running"}}`
    - `Completed` → `{"success": true, "data": {"status": "started", "message": ...}}`
      (fast empty sync; treat as started for the agent's poll loop)
    - `Err(e)` → `{"success": false, "error": ...}` (spawn failure, spec §3).
  - The in-process `SyncService` sync path is now unreachable from MCP but is **not**
    deleted (spec §3).
- **Tests FIRST:** none new here (spawn behavior + timing verified in Step 10). Keep
  the arm small; the response-shape contract is exercised by the manual test.
- **Verify:** `cargo build`
- **Commit:** "feat(mcp): sync_emails spawns rustymail-sync --reconcile, returns instantly"

### Step 8 — Hourly reconcile spawner (§5)

- **Goal:** a second background spawner ticks every `DIRTY_SYNC_INTERVAL_SECONDS`
  and runs `rustymail-sync --reconcile-dirty`.
- **Files:** `src/main.rs` — new `start_dirty_sync_spawner()` beside
  `start_sync_process_spawner` (`main.rs:381`); call it right after line 231/232.
- **Change:** copy the shape of `start_sync_process_spawner` (interval, skip first
  tick), read `DIRTY_SYNC_INTERVAL_SECONDS` (default 3600), each tick call
  `sync_spawner::spawn_sync(&["--reconcile-dirty".to_string()])`. The binary's lock
  file serializes it against the 5-minute and manual syncs (spec §5); the spawner
  itself does no DB query.
- **Accepted behavior (finding 7):** when an hourly tick collides with a sync
  already running, `spawn_sync` gets exit code 2 (`AlreadyRunning`) and that hour is
  skipped. The dirty flag persists (it is cleared only on a folder's successful
  reconcile), so the next tick picks it up — spec §7. No queuing/retry is added.
- **Migration note (finding 8):** `rustymail-sync` applies **no** migrations (it
  `SqlitePool::connect`s directly at `sync.rs:178` — no `sqlx::migrate!`). Migration
  015 is applied only by server startup via `CacheService` init
  (`sqlx::migrate!("./migrations")`, `cache.rs:177`). Therefore the server must have
  started at least once (creating the `dirty` column) before `--reconcile-dirty`
  runs standalone. In normal operation the server is always up before its own hourly
  spawner fires, so this holds; call it out for anyone running the binary by hand.
- **Tests FIRST:** none (background wiring; covered by Step 10 and existing build).
- **Verify:** `cargo build`
- **Commit:** "feat(sync): hourly --reconcile-dirty spawner (DIRTY_SYNC_INTERVAL_SECONDS)"

### Step 9 — `get_sync_status` MCP tool (§6)

- **Goal:** read-only tool so agents can poll sync state incl. `dirty`.
- **Files:**
  - `src/dashboard/api/handlers.rs` — register the tool in **both**
    `get_mcp_tools_jsonrpc_format()` (line 102) and `list_mcp_tools()` (line 1023),
    and add a thin dispatch arm in `execute_mcp_tool_inner` (before the `_` at line
    3871) that resolves `account_id` and delegates to the helper module below.
  - **New helper module** (finding 9 — do not grow the ~5100-line `handlers.rs`
    with the body; mirror the `sync_spawner` extraction):
    `src/dashboard/api/sync_status_tool.rs`, registered in `src/dashboard/api/mod.rs`,
    exposing one free async fn:
    ```rust
    /// Assemble the get_sync_status MCP response.
    pub async fn get_sync_status_tool(
        cache: &CacheService,
        account_email: &str,
        folder: Option<&str>,
    ) -> serde_json::Value;
    ```
  - `src/dashboard/services/cache.rs` — extend `get_sync_state` to include `dirty`,
    and add one direct-query method (see below).
- **Change:**
  - Tool params: `account_id` (REQUIRED), `folder` (optional) — same wording style as
    neighboring tool defs, in both lists.
  - Extend `SyncState` struct + the `get_sync_state` SELECT to include `dirty`
    (`SELECT ..., dirty FROM sync_state`); map to `bool` (0/1), default `false` when
    the row is absent.
  - **No-folder listing is net-new code (finding 2b).** REST `get_sync_status`
    (`handlers.rs:4807`) is *not* a mirror for it: it hard-defaults `folder` to
    `"INBOX"` (`handlers.rs:4833`) and never lists folders. Implement the listing
    without the in-memory-folder-cache quirk (finding 2a/2b): `get_sync_state`
    resolves the folder via `get_folder_from_cache_for_account` (in-memory LRU,
    `cache.rs:898`) and returns `None`/`"never_synced"` for any folder not currently
    in memory — wrong for a listing. So add a direct-query CacheService method that
    reads `sync_state` joined to `folders` for the account in one shot:
    ```rust
    /// `folders.account_id` IS the account email address (schema uses email as the
    /// accounts PK — no accounts.id column; migrations/001_create_schema.sql:8,61),
    /// so filter on folders directly with no accounts JOIN. Bind account_email:
    /// SELECT f.name, s.sync_status, s.last_uid_synced, s.last_full_sync,
    ///        s.last_incremental_sync, s.error_message, s.emails_synced,
    ///        s.emails_total, s.dirty
    /// FROM sync_state s JOIN folders f ON f.id = s.folder_id
    /// WHERE f.account_id = ?
    pub async fn get_all_sync_states_for_account(&self, account_id: &str)
        -> Result<Vec<(String, SyncState)>, CacheError>;
    ```
    (`get_all_cached_folders_for_account` at `cache.rs:847` returns folders but not
    their sync rows; this JOIN gives both in one query and avoids the LRU dependency.)
  - `get_sync_status_tool` behavior:
    - **with `folder`:** `cache.get_sync_state(folder, account_email)` → that folder's
      row: `status` (Idle/Syncing/Error), `emails_synced`, `emails_total`,
      `last_incremental_sync`, `last_full_sync`, `error_message`, `dirty`;
      `"never_synced"` when `None`. **Decision on the LRU quirk:** for the
      single-folder case, keep using `get_sync_state` for parity with REST, and
      accept that a folder absent from the in-memory cache reports `"never_synced"`;
      this is the pre-existing REST behavior and agents call `get_sync_status` right
      after touching a folder (so it is warm in the LRU). Do **not** re-plumb the
      single-folder path — the reviewer's concern is fully addressed for the listing,
      which is where a cold folder is likely.
    - **without `folder`:** `cache.get_all_sync_states_for_account(account_email)` →
      array of the same per-folder objects (each keyed by folder name); empty array
      when the account has no `sync_state` rows.
- **Tests FIRST:** helper-module + cache tests with seeded `sync_state` rows via
  in-memory `CacheService`:
  - `get_sync_status_with_folder_returns_fields` (incl. `dirty`).
  - `get_all_sync_states_for_account_lists_all_folders` — seed two folders with
    distinct sync rows (one dirty), assert both returned with correct fields, and
    that the result does not depend on the in-memory folder LRU being warm.
  - `get_sync_status_never_synced` (no row → `"never_synced"`).
- **Verify:** `cargo test -p rustymail get_sync_status && cargo test -p rustymail get_all_sync_states && cargo build`
- **Commit:** "feat(mcp): get_sync_status tool exposes per-folder sync state + dirty"

### Step 10 — End-to-end system test (spec §8) + final full build

- **Goal:** prove the whole flow against a live account before final sign-off.
- **Files:** none (verification only). Prerequisite:
  `cargo build --release` (server + `rustymail-sync`), restart backend clean per
  CLAUDE.md, using the real `.env` account.
- **Procedure (spec §8):**
  1. Pick a folder with a known cached message. Delete it via MCP `delete_messages`
     (or move it via `atomic_move_message`).
  2. Confirm the flag set: `sqlite3 data/email_cache.db "SELECT s.dirty, f.name FROM
     sync_state s JOIN folders f ON f.id=s.folder_id WHERE f.name='<folder>';"` → `1`.
  3. Confirm the stale row still present in cache (not yet pruned):
     `SELECT count(*) FROM emails WHERE folder_id=... AND uid=<deleted uid>;` → 1.
  4. Run `./target/release/rustymail-sync --reconcile-dirty`. Expect logs showing the
     dirty folder reconciled.
  5. Re-check: dead row gone (`count` → 0), `dirty` → 0.
  6. `get_sync_status` (MCP, with `folder`) reflects `dirty:false` and updated counts.
  7. Timing: call MCP `sync_emails` (account set) and confirm it returns in under a
     second (`status: "started"`); immediately call it again while the first is
     running and confirm `status: "already_running"` (lock held).
  8. Arg-validation: `rustymail-sync --reconcile` (no `--account`) exits non-zero.
- **Verify:** `cargo build --release && cargo test` (whole suite green) + the manual
  procedure above.
- **Commit:** "test: end-to-end verification of async sync + dirty reconcile" (docs/
  notes only if anything is recorded; code already committed in prior steps).

---

## Dependency order (why this sequence)

1 (migration) → 2 (cache helpers need the column) → 3 (EmailService hooks need the
cache helpers). 4 (sync_spawner) is independent of 1-3 but must precede 7 (MCP arm
uses it) and 8 (hourly spawner uses it). 5 (reconcile module) must precede 6 (binary
flags call into it). 6 must precede 7 (MCP spawns `--reconcile`) and 8 (hourly spawns
`--reconcile-dirty`). 9 (status tool) depends only on 1 (the `dirty` column). 10 is
last (needs 6-9 built).
