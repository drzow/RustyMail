# Mutation "no-op" report — root-cause diagnosis

Date: 2026-07-17
Status: Diagnosis (no code changed in this phase)
Branch: feat/managesieve
Investigator: architect (debugging team)

## The report

A mail-handling agent, using the rustymail MCP server against the live account
`drzow@bruggerink.com`, reported that on 2026-07-17 it:

- (a) deleted 6 historical candy-spam messages from INBOX via `delete_messages`
  + `expunge`, and
- (b) moved that day's Venmo / ias-opportunities / Endeavor Health messages out
  of INBOX via `atomic_move_message` / `atomic_batch_move`.

All calls reported success, but immediate spot-checks (`get_email_by_uid`,
`get_folder_stats`) showed nothing changed — same folder, same UID, stale
`cached_at`. Reproduced on 3 attempts. Sieve edits in the same session persisted.

## Verdict

**Not a server-side mutation bug.** The IMAP operations all succeeded at the
protocol level, the per-folder dirty flags were set correctly, and the hourly
reconcile has since healed the cache — the messages really are deleted/moved.

- **Hypothesis B (real defect in delete/expunge/batch-move paths): REFUTED.**
- **Hypothesis A (cache-verification artifact — reporter read a stale cache):
  CONFIRMED.** This is what fooled the reporter.
- **Hypothesis C (already healed by the hourly reconcile): CONFIRMED.** Cache
  and live server now agree.

The only real defects are UX/discoverability gaps in the async-sync design
shipped 2026-07-17 (`docs/superpowers/specs/2026-07-14-async-sync-dirty-flag-design.md`):
the MCP tool surface never tells an agent that cache-backed reads lag mutations,
and the one attempt the reporter made to force consistency (`sync_emails`) was
silently defeated by sync-lock contention.

## Evidence log (verbatim, UTC)

### 1. The mutations succeeded at the IMAP protocol level

From `~/.pm2/logs/rustymail-backend-error.log`, the reporter's session:

```
13:07:08  MCP POST request received
13:07:09  rustymail::imap::session  Successfully marked 6 messages as deleted      <- real IMAP STORE \Deleted
13:07:09  ...email  Successfully marked 6 emails as deleted
13:07:11  ...email  Successfully expunged messages from INBOX                       <- real IMAP EXPUNGE
13:07:11  ...email  Successfully deleted 6 messages
13:07:11  POST /mcp 200 (3.18s)                                                     <- delete_messages returned success
13:07:24  ...email  Successfully expunged messages from INBOX                       <- reporter's extra explicit expunge, also succeeded

13:09:12  rustymail::imap::atomic  Starting atomic move of UID 19170 from INBOX to Finance
13:09:13  rustymail::imap::atomic  Atomic move completed successfully
13:09:13  ...email  Successfully moved email 19170 from INBOX to Finance

13:09:14  rustymail::imap::atomic  Starting atomic batch move of 2 messages from INBOX to Archive
13:09:14  rustymail::imap::atomic  Starting atomic move of UID 19169 from INBOX to Archive
13:09:15  rustymail::imap::atomic  Atomic move completed successfully
13:09:15  rustymail::imap::atomic  Starting atomic move of UID 19166 from INBOX to Archive
13:09:15  rustymail::imap::atomic  Atomic move completed successfully
13:09:15  ...email  Successfully moved 2 emails from INBOX to Archive
```

No IMAP `NO`/`BAD`, no errors. `AtomicImapOperations::atomic_move` propagates
failure (`atomic_move(...).await?` in `email.rs:451,475`); "completed
successfully" is only logged after the server accepted the COPY+STORE+EXPUNGE.
So 6 deletes + 3 moves = **9 messages genuinely left INBOX on the server.**

### 2. The reporter's `sync_emails` was defeated by lock contention

```
13:07:23  sync_spawner  Spawned sync process (pid 924282) args=[]                  <- plain incremental (no --reconcile); does NOT prune
13:07:40  handlers  MCP sync_emails: spawning background sync for folder 'INBOX'
13:07:40  sync_spawner  Spawned sync process (pid 924553) args=["--account", "drzow@bruggerink.com", "--reconcile", "--folder", "INBOX"]
13:07:40  rustymail_sync  Another sync is already running (pid: 924282)
13:07:40  sync_spawner  Sync process reports another sync is already in progress
13:07:40  POST /mcp 200 (0.10s)  -> {"status":"already_running"}
```

The reporter did the right thing — called `sync_emails` after mutating — but a
plain incremental sync (args `[]`, which per design only fetches new UIDs and
never prunes/reconciles) already held the lock. The `--reconcile` run exited 2
(`already_running`). Per design §7 this is expected ("dirty flag persists,
nothing lost"), but the agent received `{"status":"already_running"}` with no
signal that its reconcile did not run and the cache was still stale.

### 3. The hourly reconcile healed the cache 13 minutes later

```
13:22:27  sync_spawner  Spawned sync process (pid 946796) args=["--reconcile-dirty"]
13:22:27  rustymail_sync  Reconciling dirty folders for 1 account(s)
13:22:35  rustymail::sync_reconcile  Reconcile pruned 9 dead cache row(s) from folder INBOX
13:23:51  rustymail::sync_reconcile  Reconciled folder INBOX for drzow@bruggerink.com
13:23:59  rustymail::sync_reconcile  Reconciled folder Finance for drzow@bruggerink.com
13:24:03  rustymail::sync_reconcile  Reconciled folder Archive for drzow@bruggerink.com
13:24:03  rustymail_sync  Reconcile-dirty complete, exiting
```

**Pruned exactly 9 = 6 deleted + 3 moved.** INBOX, Finance, and Archive were all
dirty (source + both move destinations), proving the dirty flags were set
correctly by the `EmailService` mutators (`email.rs:460-461,484-485,552,674`).

### 4. Current cache truth (read-only `sqlite3 file:data/email_cache.db?mode=ro`)

The `dirty` flag is a column on `sync_state` keyed by `folder_id` (schema:
`sync_state.dirty INTEGER NOT NULL DEFAULT 0`, PK `folder_id`); the table below
is a `folders`-join view for readability, not a separate store.

```
folder   dirty  status  last_incremental_sync   (folders JOIN sync_state)
INBOX    0      Idle    2026-07-17 13:43:28
Finance  0      Idle    2026-07-17 13:42:46
Archive  0      Idle    2026-07-17 13:42:49

SELECT uid FROM emails JOIN folders ... WHERE name='INBOX' AND uid IN (19166,19169,19170);
-> (0 rows)   the 3 moved UIDs are gone from the INBOX cache
```

Cache and server now agree; dirty flags cleared. Nothing is broken today.

### 5. Moved messages get fresh UIDs at the destination

An IMAP move is COPY-to-destination + STORE `\Deleted` + EXPUNGE on the source.
The destination assigns its own new UID (per the target folder's UIDNEXT); the
source UID is not preserved. So verifying a move by looking for the *original*
UID in the destination folder is meaningless — it will never be found there even
on full success. This compounds cause #1: by-UID verification fails both in the
source (row correctly gone) and in the destination (new UID). The reporter also
lost the sync lock race on **both** attempts — 13:02:33 (`--reconcile`, no
folder) and 13:07:40 (`--reconcile --folder INBOX`) — so neither forced a
reconcile.

## Root cause

Two distinct causes, both design/UX, neither a server-side mutation defect:

1. **Cache-verification artifact (what actually fooled the reporter).** By
   design (2026-07-14 spec §2, "non-goal: updating the cache inline at mutation
   time"), MCP mutations touch only IMAP + set a dirty flag; they never update
   the SQLite cache. The read tools the reporter used to verify are 100%
   cache-backed: `get_email_by_uid` ("Get full cached email by UID",
   `handlers.rs:426`), `get_folder_stats` ("Get statistics about cached folder",
   `handlers.rs:488`), `count_emails_in_folder`, `list_cached_emails`. So a read
   immediately after a successful mutation is *expected* to show the old
   folder/UID/`cached_at` until a reconcile runs. **No tool description, and no
   mutation success response, states this.** An agent cannot know it must not
   verify via cached reads.

2. **`sync_emails` cannot reliably force prompt consistency.** The `--reconcile`
   run the MCP arm spawns (`handlers.rs:3632-3670`) shares one lock file with
   the 5-minute incremental spawner and the hourly reconcile. When any of those
   holds the lock, `sync_emails` returns `{"status":"already_running"}`
   (`handlers.rs:3652`) and the reconcile is skipped. The agent is not told that
   (a) its reconcile did not run and (b) the folder is still dirty / cache still
   stale. Cache still heals on the next hourly tick (no data loss), but the
   agent's deliberate attempt at immediate verification is silently defeated.

## Fix proposal (minimal, respects repo simplicity rules)

No server mutation logic needs to change — it is correct. Fix the tool surface
so agents stop being fooled and have a *reliable* verification path. All edits
are in `src/dashboard/api/handlers.rs`. To avoid repeating long literals (repo
rules 8 "highest abstraction" and no-hardcoding), define two module-level
`const &str` and reference them; this also keeps both description blocks in sync.

### Consts (define once near the top of the tool module)

```rust
/// Appended to every mutating tool's success payload and description.
const MUTATION_CACHE_NOTE: &str = "Change applied on the IMAP server. \
Cache-backed reads (get_email_by_uid, get_email_by_index, get_folder_stats, \
count_emails_in_folder, list_cached_emails) will NOT show it until the folder \
is reconciled. To confirm now: call sync_emails and retry it until it returns \
status \"started\" — a \"already_running\" reply means your reconcile did NOT \
run and the cache is still stale (a plain 5-minute incremental sync never \
clears the dirty flag). Once you get \"started\", poll get_sync_status until \
the folder's dirty flag is 0. Otherwise it reconciles automatically within the \
hour. Do not verify by the original UID: a moved message gets a NEW UID at the \
destination, so the old UID is absent from both source and destination.";

/// Appended to every cache-backed read tool's description.
const CACHE_READ_NOTE: &str = " Reads the local cache, which can lag the server \
after a mutation until the folder is reconciled; if you just mutated this \
folder, confirm via get_sync_status (dirty must be 0) rather than trusting this \
result.";
```

### Fix (a) — cache-lag `note` on the 8 mutation SUCCESS payloads  [primary]

In each arm's `Ok(_) =>` branch, add `"note": MUTATION_CACHE_NOTE` to the
existing `"data": { ... }` object. This lands exactly when the agent decides how
to verify — the strongest signal. Exact arms/lines (the `data` object to extend):

| Tool | arm | `data` block to add `note` to |
|---|---|---|
| `atomic_move_message` | 2104 | 2134-2138 |
| `atomic_batch_move` | 2151 | 2189-2194 |
| `mark_as_read` | 2207 | 2237-2241 |
| `mark_as_unread` | 2254 | 2284-2288 |
| `mark_as_deleted` | 2301 | 2331-2335 |
| `delete_messages` | 2348 | 2378-2382 |
| `undelete_messages` | 2395 | 2424-2428 |
| `expunge` | 2442 | 2456-2458 |

Example (`atomic_move_message`, 2134-2138):
```rust
"data": {
    "uid": uid,
    "from_folder": from_folder,
    "to_folder": to_folder,
    "note": MUTATION_CACHE_NOTE
},
```

### Fix (b) — append `MUTATION_CACHE_NOTE` to the 8 mutating tool descriptions

Same 8 tools, both registration blocks (`tools/list` ~152-345 and the second
block ~1105-1222 — fix every instance per repo rule 11). Change each
`"description": "<existing>"` to `"description": format!("{} {}", "<existing>",
MUTATION_CACHE_NOTE)`. Weaker than (a) but free and helps agents plan before
calling.

### Fix (c) — flag the read tools as cache-backed  [wording corrected]

Append `CACHE_READ_NOTE` to the descriptions of `get_email_by_uid`,
`get_email_by_index`, `get_folder_stats`, `count_emails_in_folder`,
`list_cached_emails` (both blocks; descriptions at ~426, ~448, ~488, ~470, ~400
and their ~1105-1222 twins). Same `format!` pattern.

### Fix (d) — make `sync_emails` `already_running` honest  [blocking correction]

`handlers.rs:3652-3656`. The current response is bare `{"status":"already_running"}`.
Add a `message` that tells the agent its reconcile did NOT run and gives the
**retry-then-poll** path (polling dirty alone is a trap — only reconcile clears
it, and the running sync may be a non-reconciling incremental):

```rust
Ok(SpawnOutcome::AlreadyRunning) => serde_json::json!({
    "success": true,
    "data": {
        "status": "already_running",
        "message": "A sync already holds the lock, so your reconcile did NOT \
start. That running sync may be a plain incremental, which never clears dirty \
flags. Retry sync_emails until it returns status \"started\", then poll \
get_sync_status until the target folder's dirty flag is 0. (Any dirty folders \
are also reconciled automatically each hour.)"
    },
    "tool": tool_name
}),
```

Rationale for retry (reviewer's lock-occupancy measurement): the lock is held
~55-75s per 300s incremental cadence, i.e. free ~78% of the time, so 1-2 retries
of `sync_emails` almost always land a real `started`. The log proves polling
dirty without retrying is insufficient: incrementals ran 13:12 and 13:17 yet
`dirty` stayed set until the 13:22 `--reconcile-dirty` pruned 9.

### Deferred (agreed, conditional on Fix (d) above)

- `sync_emails` returning current dirty/sync status inline in its response.
- A reconcile queue the in-flight sync honors on exit.

Both are larger changes than the gap warrants for MVP; retry-then-poll (Fix (d))
already gives agents a reliable path. Optional trivial nicety, author's call:
include the account's current dirty state in the `started`/`already_running`
payload — skip if it grows the arm meaningfully.

### Expected-vs-actual, restated

- Expected (server): 6 messages expunged from INBOX; UIDs 19170→Finance,
  19169+19166→Archive. **Actual: exactly that** (log §1, prune count §3, cache §4).
- Expected (cache, immediately after mutation, by design): unchanged until
  reconcile. **Actual: unchanged** — which the reporter misread as failure.
- Expected (after `sync_emails`): reporter expected a reconcile; **actual:
  `already_running`, reconcile skipped**, cache stayed stale until 13:22 hourly tick.

## Safety note

Investigation used read-only operations only (log reads, `sqlite3 ?mode=ro`).
No mutations were issued against `drzow@bruggerink.com`; no reversals were
needed. Services were not restarted.
