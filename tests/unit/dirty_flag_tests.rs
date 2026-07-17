// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Edge cases for CacheService's dirty-flag upsert helpers that the existing
//! cache_service_tests.rs (already >500 lines) does not cover: clearing a folder
//! that has no sync_state row yet, and idempotent double-marking. Uses file-based
//! temp DBs (never the live cache DB).

use rustymail::dashboard::services::cache::{CacheService, CacheConfig};
use serial_test::serial;
use std::fs;

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

async fn setup(test_name: &str, account: &str) -> CacheService {
    let mut service = CacheService::new(create_test_config(test_name));
    service.initialize().await.unwrap();
    let pool = service.db_pool.as_ref().unwrap();
    sqlx::query(
        r#"INSERT INTO accounts (email_address, display_name, imap_host, imap_port, imap_user, imap_pass)
           VALUES (?, ?, 'test.imap.com', 993, ?, 'pw')"#,
    )
    .bind(account)
    .bind(format!("Acct {}", account))
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    service
}

/// dirty value + number of sync_state rows for a folder (via JOIN, LRU-independent).
async fn dirty_and_row_count(service: &CacheService, folder: &str, account: &str) -> (Option<i64>, i64) {
    let pool = service.db_pool.as_ref().unwrap();
    let dirty: Option<i64> = sqlx::query_scalar(
        "SELECT s.dirty FROM sync_state s JOIN folders f ON f.id = s.folder_id
         WHERE f.name = ? AND f.account_id = ?",
    )
    .bind(folder)
    .bind(account)
    .fetch_optional(pool)
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_state s JOIN folders f ON f.id = s.folder_id
         WHERE f.name = ? AND f.account_id = ?",
    )
    .bind(folder)
    .bind(account)
    .fetch_one(pool)
    .await
    .unwrap();
    (dirty, count)
}

// clear_folder_dirty on a folder that never had a sync_state row: the upsert
// (INSERT ... ON CONFLICT) must create the row at dirty=0 rather than erroring
// or leaving a stale/absent flag. The existing clear test only clears after a
// mark, so the INSERT branch of clear_folder_dirty is otherwise untested.
#[tokio::test]
#[serial]
async fn clear_folder_dirty_on_missing_row_creates_clean_row() {
    let test_name = "clear_dirty_missing_row";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let service = setup(test_name, account).await;

    service.clear_folder_dirty("INBOX", account).await.unwrap();

    let (dirty, count) = dirty_and_row_count(&service, "INBOX", account).await;
    assert_eq!(dirty, Some(0), "clear on a missing row must upsert dirty=0");
    assert_eq!(count, 1, "exactly one sync_state row created");
    cleanup_test_db(test_name);
}

// Marking dirty twice must be idempotent: still dirty=1, still exactly one
// sync_state row (the ON CONFLICT branch must UPDATE, never duplicate the row).
#[tokio::test]
#[serial]
async fn mark_folder_dirty_twice_is_idempotent() {
    let test_name = "mark_dirty_twice";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let service = setup(test_name, account).await;

    service.mark_folder_dirty("INBOX", account).await.unwrap();
    service.mark_folder_dirty("INBOX", account).await.unwrap();

    let (dirty, count) = dirty_and_row_count(&service, "INBOX", account).await;
    assert_eq!(dirty, Some(1), "still dirty after a second mark");
    assert_eq!(count, 1, "double mark must not create a duplicate sync_state row");
    cleanup_test_db(test_name);
}
