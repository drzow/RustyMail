// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Unit tests for the get_sync_status MCP tool backing logic and the
//! get_all_sync_states_for_account cache query. Uses file-based temp DBs
//! (never the live cache DB).

use rustymail::dashboard::services::cache::{CacheService, CacheConfig};
use rustymail::dashboard::api::sync_status_tool::get_sync_status_tool;
use sqlx::SqlitePool;
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

async fn setup(test_name: &str, account: &str) -> (CacheService, SqlitePool) {
    let mut service = CacheService::new(create_test_config(test_name));
    service.initialize().await.unwrap();
    let pool = service.db_pool.as_ref().unwrap().clone();
    sqlx::query(
        r#"INSERT INTO accounts (email_address, display_name, imap_host, imap_port, imap_user, imap_pass)
           VALUES (?, ?, 'test.imap.com', 993, ?, 'pw')"#,
    )
    .bind(account)
    .bind(format!("Acct {}", account))
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();
    (service, pool)
}

/// Seed a folder + sync_state row directly via SQL (bypasses the folder LRU, so
/// the listing test proves it does not depend on a warm cache).
async fn seed_folder_with_state(
    pool: &SqlitePool,
    account: &str,
    folder: &str,
    status: &str,
    synced: i64,
    total: i64,
    dirty: i64,
) {
    let folder_id: i64 = sqlx::query_scalar(
        "INSERT INTO folders (account_id, name, attributes) VALUES (?, ?, '[]') RETURNING id",
    )
    .bind(account)
    .bind(folder)
    .fetch_one(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sync_state (folder_id, last_uid_synced, sync_status, emails_synced, emails_total, dirty)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(folder_id)
    .bind(42i64)
    .bind(status)
    .bind(synced)
    .bind(total)
    .bind(dirty)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn get_sync_status_with_folder_returns_fields() {
    let test_name = "status_with_folder";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (service, pool) = setup(test_name, account).await;
    seed_folder_with_state(&pool, account, "INBOX", "Idle", 350, 1600, 1).await;

    let out = get_sync_status_tool(&service, account, Some("INBOX")).await;
    assert_eq!(out["folder"], "INBOX");
    assert_eq!(out["status"], "Idle");
    assert_eq!(out["emails_synced"], 350);
    assert_eq!(out["emails_total"], 1600);
    assert_eq!(out["dirty"], true, "dirty flag must be reflected");
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn get_all_sync_states_for_account_lists_all_folders() {
    let test_name = "status_list_all";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (service, pool) = setup(test_name, account).await;
    // Seed via SQL only — nothing is warmed in the folder LRU.
    seed_folder_with_state(&pool, account, "INBOX", "Idle", 10, 10, 0).await;
    seed_folder_with_state(&pool, account, "Archive", "Syncing", 5, 20, 1).await;

    let out = get_sync_status_tool(&service, account, None).await;
    let folders = out["folders"].as_array().expect("folders array");
    assert_eq!(folders.len(), 2, "both folders listed regardless of LRU state");

    let inbox = folders.iter().find(|f| f["folder"] == "INBOX").unwrap();
    assert_eq!(inbox["status"], "Idle");
    assert_eq!(inbox["dirty"], false);

    let archive = folders.iter().find(|f| f["folder"] == "Archive").unwrap();
    assert_eq!(archive["status"], "Syncing");
    assert_eq!(archive["emails_total"], 20);
    assert_eq!(archive["dirty"], true);
    cleanup_test_db(test_name);
}

#[tokio::test]
#[serial]
async fn get_sync_status_never_synced() {
    let test_name = "status_never_synced";
    cleanup_test_db(test_name);
    let account = "a@test.com";
    let (service, _pool) = setup(test_name, account).await;

    // No folder / sync_state row exists for this account.
    let out = get_sync_status_tool(&service, account, Some("INBOX")).await;
    assert_eq!(out["status"], "never_synced");
    assert_eq!(out["dirty"], false);
    cleanup_test_db(test_name);
}
