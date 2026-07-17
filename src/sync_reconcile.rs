// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pool-based cache reconcile logic, shared by the `rustymail-sync` binary.
//!
//! Reconcile of one folder = SEARCH ALL for the live UID set -> prune dead cache
//! rows -> refresh cached flags -> clear the folder's dirty flag. The pure-DB
//! core (`reconcile_cache`) takes an already-fetched live-UID set and flag
//! updates so it is unit-testable without an IMAP connection; `reconcile_folder`
//! is the thin IMAP wrapper on top of it.
//!
//! These are free functions over a raw `&SqlitePool` (not `CacheService`)
//! because the binary works directly on a pool and has no memory cache to
//! evict.

use log::{debug, info};
use sqlx::SqlitePool;

use crate::imap::client::ImapClient;
use crate::imap::session::AsyncImapSessionWrapper;

/// Get or create a folder_id for the given folder_name and account_id.
/// (Moved from src/bin/sync.rs so both the binary and reconcile share one copy.)
pub async fn get_or_create_folder_id(
    pool: &SqlitePool,
    folder_name: &str,
    account_id: &str,
) -> Result<i64, sqlx::Error> {
    // First try to get existing folder
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM folders WHERE name = ? AND account_id = ?"
    )
    .bind(folder_name)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create the folder
    sqlx::query(
        "INSERT INTO folders (name, account_id, created_at) VALUES (?, ?, datetime('now'))"
    )
    .bind(folder_name)
    .bind(account_id)
    .execute(pool)
    .await?;

    // Get the new ID
    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM folders WHERE name = ? AND account_id = ?"
    )
    .bind(folder_name)
    .bind(account_id)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Remove cache rows for messages no longer present in the live folder.
///
/// MUST only be called with `live_uids` being the complete set of UIDs currently
/// in the folder (search "ALL"). The set difference (cached − live) is the dead
/// rows left behind by server-side moves/deletes, which otherwise inflate
/// get_folder_stats and counts. Deletes are chunked to stay well under SQLite's
/// bound-parameter limit.
/// (Moved from src/bin/sync.rs — no logic change.)
pub async fn prune_dead_rows(
    pool: &SqlitePool,
    folder_name: &str,
    account_email: &str,
    live_uids: &[u32],
) -> Result<usize, sqlx::Error> {
    let folder_id = get_or_create_folder_id(pool, folder_name, account_email).await?;

    let cached: Vec<u32> = sqlx::query_scalar::<_, i64>(
        "SELECT uid FROM emails WHERE folder_id = ?"
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|u| u as u32)
    .collect();

    let live: std::collections::HashSet<u32> = live_uids.iter().copied().collect();
    let dead: Vec<u32> = cached.into_iter().filter(|u| !live.contains(u)).collect();

    if dead.is_empty() {
        return Ok(0);
    }

    for chunk in dead.chunks(500) {
        let placeholders = std::iter::repeat("?").take(chunk.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "DELETE FROM emails WHERE folder_id = ? AND uid IN ({})",
            placeholders
        );
        let mut q = sqlx::query(&sql).bind(folder_id);
        for uid in chunk {
            q = q.bind(*uid as i64);
        }
        q.execute(pool).await?;
    }

    Ok(dead.len())
}

/// Update only the flags for one cached email. Serializes flags EXACTLY like
/// CacheService::update_email_flags (cache.rs:509-511): BTreeSet dedup, then
/// serde_json::to_string — because emails.flags is a JSON-array TEXT column and
/// readers expect that shape. No memory-cache eviction (the binary has none).
pub async fn update_cached_flags(
    pool: &SqlitePool,
    folder_id: i64,
    uid: u32,
    flags: &[String],
) -> Result<(), sqlx::Error> {
    let deduped: Vec<&str> = flags.iter().map(|s| s.as_str())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let flags_json = serde_json::to_string(&deduped).unwrap_or_else(|_| "[]".to_string());

    sqlx::query("UPDATE emails SET flags = ?, updated_at = CURRENT_TIMESTAMP WHERE folder_id = ? AND uid = ?")
        .bind(&flags_json)
        .bind(folder_id)
        .bind(uid as i64)
        .execute(pool)
        .await?;
    Ok(())
}

/// All dirty folders across all accounts. `folders.account_id` IS the account
/// email address (schema uses email as the accounts PK — no accounts.id column;
/// migrations/001_create_schema.sql:8,61), so no accounts JOIN is needed.
/// Returns (account_email, folder_name).
pub async fn list_dirty_folders(pool: &SqlitePool) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT f.account_id, f.name FROM sync_state s
         JOIN folders f ON f.id = s.folder_id
         WHERE s.dirty = 1"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Clear a folder's dirty flag by folder_id.
pub async fn clear_dirty(pool: &SqlitePool, folder_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sync_state SET dirty = 0, updated_at = CURRENT_TIMESTAMP WHERE folder_id = ?")
        .bind(folder_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a reconcile failure on the folder's sync_state row (spec §7): sets
/// sync_status = 'error' (the exact lowercase casing get_sync_state maps to
/// SyncStatus::Error) and error_message, so get_sync_status can surface it.
/// Deliberately leaves `dirty` untouched (stays 1 so the next tick retries) and
/// does not touch last_uid_synced. Upsert mirrors the sync_state ON CONFLICT
/// pattern; best-effort — callers log if this itself fails.
pub async fn mark_sync_error(
    pool: &SqlitePool,
    folder_name: &str,
    account_email: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    let folder_id = get_or_create_folder_id(pool, folder_name, account_email).await?;
    sqlx::query(
        "INSERT INTO sync_state (folder_id, sync_status, error_message, updated_at)
         VALUES (?, 'error', ?, datetime('now'))
         ON CONFLICT(folder_id) DO UPDATE SET
             sync_status = 'error',
             error_message = excluded.error_message,
             updated_at = datetime('now')"
    )
    .bind(folder_id)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// PURE DB reconcile (no IMAP) — the unit-testable core:
/// 1. prune_dead_rows against `live_uids`
/// 2. update_cached_flags for each (uid, flags) in `flag_updates`
/// 3. clear_dirty(folder_id) — LAST, and only if 1 & 2 both succeeded.
///
/// Returns Err (WITHOUT clearing dirty) if any step fails, so a failed reconcile
/// leaves the folder dirty for the next tick (spec §7).
pub async fn reconcile_cache(
    pool: &SqlitePool,
    folder_id: i64,
    account_email: &str,
    folder_name: &str,
    live_uids: &[u32],
    flag_updates: &[(u32, Vec<String>)],
) -> Result<(), sqlx::Error> {
    let pruned = prune_dead_rows(pool, folder_name, account_email, live_uids).await?;
    if pruned > 0 {
        info!("Reconcile pruned {} dead cache row(s) from folder {}", pruned, folder_name);
    }

    for (uid, flags) in flag_updates {
        update_cached_flags(pool, folder_id, *uid, flags).await?;
    }

    clear_dirty(pool, folder_id).await?;
    debug!("Reconcile cleared dirty flag for folder {} ({})", folder_name, account_email);
    Ok(())
}

/// IMAP wrapper: select + SEARCH ALL + fetch flags (chunks of 500), then delegate
/// to reconcile_cache. Any IMAP error returns Err before reconcile_cache runs, so
/// the dirty flag is untouched (spec §7).
pub async fn reconcile_folder(
    pool: &SqlitePool,
    client: &ImapClient<AsyncImapSessionWrapper>,
    account_email: &str,
    folder_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let folder_id = get_or_create_folder_id(pool, folder_name, account_email).await?;

    // Select the folder and get the complete live UID set.
    client.select_folder(folder_name).await?;
    let live_uids = client.search_emails("ALL").await?;

    // Refresh flags for the UIDs still cached (after the impending prune, the dead
    // ones no longer matter). Read cached UIDs, then fetch flags in chunks of 500.
    let cached_uids: Vec<u32> = sqlx::query_scalar::<_, i64>(
        "SELECT uid FROM emails WHERE folder_id = ?"
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|u| u as u32)
    .collect();

    let mut flag_updates: Vec<(u32, Vec<String>)> = Vec::new();
    for chunk in cached_uids.chunks(500) {
        let flags = client.fetch_flags(chunk).await?;
        flag_updates.extend(flags);
    }

    reconcile_cache(pool, folder_id, account_email, folder_name, &live_uids, &flag_updates).await?;
    info!("Reconciled folder {} for {}", folder_name, account_email);
    Ok(())
}
