// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Unit tests for the pure-DB reconcile core (rustymail::sync_reconcile).
//! No IMAP: the split between reconcile_cache (pure DB) and reconcile_folder
//! (IMAP wrapper) exists precisely so this logic can be tested against an
//! in-memory-style pool. Uses file-based temp DBs (never the live cache DB).

use rustymail::dashboard::services::cache::{CacheService, CacheConfig, SyncStatus};
use rustymail::sync_reconcile::{
    clear_dirty, get_or_create_folder_id, list_dirty_folders, mark_sync_error, prune_dead_rows,
    reconcile_cache, update_cached_flags,
};
use sqlx::{Row, SqlitePool};
use std::fs;
use serial_test::serial;

fn create_test_config(test_name: &str) -> CacheConfig {
    CacheConfig {
        database_url: format!("sqlite:test_data/{}_test.db", test_name),
        max_memory_items: 100,
        max_folder_items: 50,
        max_cache_size_mb: 100,
        max_email_age_days: 30,
        sync_interval_seconds: 300,
    }
}

fn cleanup_test_db(test_name: &str) {
    let db_path = format!("test_data/{}_test.db", test_name);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{}-shm", db_path));
    let _ = fs::remove_file(format!("{}-wal", db_path));
}

/// Initialize a migrated pool (via CacheService) and seed one account.
async fn setup_pool(test_name: &str, account_id: &str) -> (CacheService, SqlitePool) {
    let mut service = CacheService::new(create_test_config(test_name));
    service.initialize().await.unwrap();
    let pool = service.db_pool.as_ref().unwrap().clone();
    sqlx::query(
        r#"INSERT INTO accounts (email_address, display_name, imap_host, imap_port, imap_user, imap_pass)
           VALUES (?, ?, 'test.imap.com', 993, ?, 'pw')"#,
    )
    .bind(account_id)
    .bind(format!("Acct {}", account_id))
    .bind(account_id)
    .execute(&pool)
    .await
    .unwrap();
    (service, pool)
}

/// Insert a cached email row with the given uid and flags-json.
async fn seed_email(pool: &SqlitePool, folder_id: i64, uid: u32, flags_json: &str) {
    sqlx::query("INSERT INTO emails (folder_id, uid, subject, flags) VALUES (?, ?, ?, ?)")
        .bind(folder_id)
        .bind(uid as i64)
        .bind(format!("subj {}", uid))
        .bind(flags_json)
        .execute(pool)
        .await
        .unwrap();
}

async fn set_dirty(pool: &SqlitePool, folder_id: i64, dirty: i64) {
    sqlx::query(
        "INSERT INTO sync_state (folder_id, dirty) VALUES (?, ?)
         ON CONFLICT(folder_id) DO UPDATE SET dirty = excluded.dirty",
    )
    .bind(folder_id)
    .bind(dirty)
    .execute(pool)
    .await
    .unwrap();
}

async fn read_dirty(pool: &SqlitePool, folder_id: i64) -> i64 {
    sqlx::query_scalar("SELECT dirty FROM sync_state WHERE folder_id = ?")
        .bind(folder_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn read_uids(pool: &SqlitePool, folder_id: i64) -> Vec<u32> {
    let mut uids: Vec<u32> = sqlx::query_scalar::<_, i64>("SELECT uid FROM emails WHERE folder_id = ?")
        .bind(folder_id)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|u| u as u32)
        .collect();
    uids.sort();
    uids
}

#[tokio::test]
#[serial]
async fn reconcile_cache_prunes_updates_and_clears_dirty() {
    let test_name = "reconcile_happy";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    // uids 1,2,3 cached; 2 is dead (not in live set).
    seed_email(&pool, folder_id, 1, "[\"\\\\Seen\"]").await;
    seed_email(&pool, folder_id, 2, "[]").await;
    seed_email(&pool, folder_id, 3, "[]").await;
    set_dirty(&pool, folder_id, 1).await;

    let live_uids = vec![1u32, 3u32];
    let flag_updates = vec![
        (1u32, vec!["\\Seen".to_string()]),
        (3u32, vec!["\\Flagged".to_string(), "\\Seen".to_string()]),
    ];

    reconcile_cache(&pool, folder_id, account, "INBOX", &live_uids, &flag_updates)
        .await
        .unwrap();

    // Dead row 2 pruned; 1 and 3 survive.
    assert_eq!(read_uids(&pool, folder_id).await, vec![1, 3], "dead row must be pruned");

    // Flags rewritten (BTreeSet-deduped JSON) for uid 3.
    let flags3: String = sqlx::query("SELECT flags FROM emails WHERE folder_id = ? AND uid = 3")
        .bind(folder_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("flags");
    assert_eq!(flags3, "[\"\\\\Flagged\",\"\\\\Seen\"]", "flags must be sorted/deduped JSON");

    // Dirty cleared last.
    assert_eq!(read_dirty(&pool, folder_id).await, 0, "dirty must be cleared on success");
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn reconcile_cache_leaves_dirty_on_failure() {
    let test_name = "reconcile_fail";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    set_dirty(&pool, folder_id, 1).await;

    // Force a SQL error: drop the emails table so prune_dead_rows' SELECT fails.
    sqlx::query("DROP TABLE emails").execute(&pool).await.unwrap();

    let result = reconcile_cache(&pool, folder_id, account, "INBOX", &[1u32], &[]).await;
    assert!(result.is_err(), "reconcile_cache must error when emails table is gone");

    // clear_dirty was never reached -> dirty survives for the next tick.
    assert_eq!(read_dirty(&pool, folder_id).await, 1, "dirty must survive a failed reconcile");
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn list_dirty_folders_returns_marked() {
    let test_name = "list_dirty";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let inbox = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    let archive = get_or_create_folder_id(&pool, "Archive", account).await.unwrap();
    let sent = get_or_create_folder_id(&pool, "Sent", account).await.unwrap();
    set_dirty(&pool, inbox, 1).await;
    set_dirty(&pool, archive, 0).await;
    set_dirty(&pool, sent, 1).await;

    let mut dirty = list_dirty_folders(&pool).await.unwrap();
    dirty.sort();
    assert_eq!(
        dirty,
        vec![
            (account.to_string(), "INBOX".to_string()),
            (account.to_string(), "Sent".to_string()),
        ],
        "only dirty folders returned with correct email + name"
    );
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn clear_dirty_resets_row() {
    let test_name = "clear_dirty_fn";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    set_dirty(&pool, folder_id, 1).await;
    clear_dirty(&pool, folder_id).await.unwrap();
    assert_eq!(read_dirty(&pool, folder_id).await, 0);
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn update_cached_flags_writes_json() {
    let test_name = "update_flags_json";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    seed_email(&pool, folder_id, 7, "[]").await;

    // Duplicate + unsorted input must serialize as a sorted, deduped JSON array.
    update_cached_flags(&pool, folder_id, 7, &[
        "\\Seen".to_string(),
        "\\Answered".to_string(),
        "\\Seen".to_string(),
    ])
    .await
    .unwrap();

    let flags: String = sqlx::query("SELECT flags FROM emails WHERE folder_id = ? AND uid = 7")
        .bind(folder_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("flags");
    assert_eq!(flags, "[\"\\\\Answered\",\"\\\\Seen\"]", "BTreeSet dedup + sort");
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn prune_dead_rows_removes_absent_uids() {
    let test_name = "prune_absent";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    for uid in [1u32, 2, 3, 4, 5] {
        seed_email(&pool, folder_id, uid, "[]").await;
    }

    let live = vec![1u32, 3, 5];
    let removed = prune_dead_rows(&pool, "INBOX", account, &live).await.unwrap();
    assert_eq!(removed, 2, "uids 2 and 4 are dead");
    assert_eq!(read_uids(&pool, folder_id).await, vec![1, 3, 5]);

    // Second prune with the same live set is a no-op.
    let removed_again = prune_dead_rows(&pool, "INBOX", account, &live).await.unwrap();
    assert_eq!(removed_again, 0);
    cleanup_test_db(test_name);
}

// An empty live-UID set means the server folder is now empty (everything was
// moved/deleted). reconcile_cache MUST prune every cached row — it must not
// mistake "SEARCH ALL returned nothing" for "nothing changed, skip pruning".
#[tokio::test]
#[serial]
async fn reconcile_cache_empty_live_uids_prunes_all() {
    let test_name = "reconcile_empty_live";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    for uid in [1u32, 2, 3] {
        seed_email(&pool, folder_id, uid, "[]").await;
    }
    set_dirty(&pool, folder_id, 1).await;

    reconcile_cache(&pool, folder_id, account, "INBOX", &[], &[])
        .await
        .unwrap();

    assert!(
        read_uids(&pool, folder_id).await.is_empty(),
        "an empty live set must prune every cached row"
    );
    assert_eq!(read_dirty(&pool, folder_id).await, 0, "dirty cleared after emptying folder");
    cleanup_test_db(test_name);
}

// A UID that was just pruned may still appear in flag_updates (the flags were
// fetched before the prune decision). prune runs first, so the follow-up
// UPDATE hits a now-absent row: it must be a silent no-op (0 rows affected,
// no error) and must NOT resurrect the dead row. The surviving UID's flags
// still get written.
#[tokio::test]
#[serial]
async fn reconcile_cache_flag_update_for_pruned_uid_is_harmless() {
    let test_name = "reconcile_flag_pruned";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account).await;

    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    seed_email(&pool, folder_id, 1, "[]").await; // live
    seed_email(&pool, folder_id, 2, "[]").await; // dead -> pruned
    set_dirty(&pool, folder_id, 1).await;

    let live = vec![1u32];
    let flag_updates = vec![
        (1u32, vec!["\\Seen".to_string()]),
        (2u32, vec!["\\Seen".to_string()]), // for the just-pruned uid
    ];
    reconcile_cache(&pool, folder_id, account, "INBOX", &live, &flag_updates)
        .await
        .unwrap();

    assert_eq!(
        read_uids(&pool, folder_id).await,
        vec![1],
        "flag update for a pruned uid must not resurrect it"
    );
    let flags1: String = sqlx::query("SELECT flags FROM emails WHERE folder_id = ? AND uid = 1")
        .bind(folder_id)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("flags");
    assert_eq!(flags1, "[\"\\\\Seen\"]", "surviving uid flags still updated");
    assert_eq!(read_dirty(&pool, folder_id).await, 0);
    cleanup_test_db(test_name);
}

// The hourly reconcile groups dirty folders by account, so list_dirty_folders
// must attribute each dirty folder to the RIGHT account — even when two
// accounts have a dirty folder with the identical name (INBOX). A single-account
// test cannot catch a JOIN that drops or crosses account attribution.
#[tokio::test]
#[serial]
async fn list_dirty_folders_spans_multiple_accounts() {
    let test_name = "list_dirty_multi_acct";
    cleanup_test_db(test_name);
    let account_a = "a@test.com";
    let (_svc, pool) = setup_pool(test_name, account_a).await;

    let account_b = "b@test.com";
    sqlx::query(
        r#"INSERT INTO accounts (email_address, display_name, imap_host, imap_port, imap_user, imap_pass)
           VALUES (?, ?, 'test.imap.com', 993, ?, 'pw')"#,
    )
    .bind(account_b)
    .bind("Acct B")
    .bind(account_b)
    .execute(&pool)
    .await
    .unwrap();

    let a_inbox = get_or_create_folder_id(&pool, "INBOX", account_a).await.unwrap();
    let b_inbox = get_or_create_folder_id(&pool, "INBOX", account_b).await.unwrap();
    let b_sent = get_or_create_folder_id(&pool, "Sent", account_b).await.unwrap();
    set_dirty(&pool, a_inbox, 1).await;
    set_dirty(&pool, b_inbox, 1).await;
    set_dirty(&pool, b_sent, 0).await; // clean -> must be excluded

    let mut dirty = list_dirty_folders(&pool).await.unwrap();
    dirty.sort();
    assert_eq!(
        dirty,
        vec![
            (account_a.to_string(), "INBOX".to_string()),
            (account_b.to_string(), "INBOX".to_string()),
        ],
        "each dirty folder attributed to its own account despite same folder name"
    );
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn mark_sync_error_sets_error_status_and_keeps_dirty() {
    let test_name = "mark_sync_error";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (svc, pool) = setup_pool(test_name, account).await;

    // Seed a folder + sync_state row that is dirty and currently Idle.
    let folder_id = get_or_create_folder_id(&pool, "INBOX", account).await.unwrap();
    set_dirty(&pool, folder_id, 1).await;

    mark_sync_error(&pool, "INBOX", account, "boom: reconcile blew up").await.unwrap();

    // Read back through the service so we exercise the string->SyncStatus mapping.
    let states = svc.get_all_sync_states_for_account(account).await.unwrap();
    let (_, state) = states.iter().find(|(name, _)| name == "INBOX").unwrap();
    assert_eq!(state.sync_status, SyncStatus::Error, "status must map to Error");
    assert_eq!(state.error_message.as_deref(), Some("boom: reconcile blew up"), "error message stored");

    // dirty must be untouched (still 1) so the next tick retries.
    assert_eq!(read_dirty(&pool, folder_id).await, 1, "dirty must stay set on error");
    cleanup_test_db(test_name);
}
