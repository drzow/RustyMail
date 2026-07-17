// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Backing logic for the `get_sync_status` MCP tool.
//!
//! Kept out of the ~5000-line handlers.rs (mirrors the sync_spawner extraction):
//! one free async function that assembles the response from CacheService sync
//! state. Agents call `sync_emails`, then poll this until `status` is `Idle`.

use serde_json::{json, Value};

use crate::dashboard::services::cache::{CacheService, SyncState};

/// Serialize one SyncState into the per-folder response object.
fn sync_state_to_json(folder: &str, s: &SyncState) -> Value {
    json!({
        "folder": folder,
        "status": format!("{:?}", s.sync_status),
        "last_uid_synced": s.last_uid_synced,
        "last_full_sync": s.last_full_sync,
        "last_incremental_sync": s.last_incremental_sync,
        "error_message": s.error_message,
        "emails_synced": s.emails_synced,
        "emails_total": s.emails_total,
        "dirty": s.dirty,
    })
}

/// The "no sync_state row" object for a specific folder.
fn never_synced_json(folder: &str) -> Value {
    json!({
        "folder": folder,
        "status": "never_synced",
        "last_uid_synced": null,
        "last_full_sync": null,
        "last_incremental_sync": null,
        "error_message": null,
        "emails_synced": 0,
        "emails_total": 0,
        "dirty": false,
    })
}

/// Assemble the get_sync_status MCP response.
///
/// With `folder`: that folder's row (or `"never_synced"` when absent). Without
/// `folder`: a `folders` array with one object per folder that has a sync_state
/// row (empty array when none).
pub async fn get_sync_status_tool(
    cache: &CacheService,
    account_email: &str,
    folder: Option<&str>,
) -> Value {
    match folder {
        Some(f) => match cache.get_sync_state(f, account_email).await {
            Ok(Some(state)) => sync_state_to_json(f, &state),
            Ok(None) => never_synced_json(f),
            Err(e) => json!({ "error": format!("Failed to get sync status: {}", e) }),
        },
        None => match cache.get_all_sync_states_for_account(account_email).await {
            Ok(states) => {
                let folders: Vec<Value> = states
                    .iter()
                    .map(|(name, state)| sync_state_to_json(name, state))
                    .collect();
                json!({ "folders": folders })
            }
            Err(e) => json!({ "error": format!("Failed to get sync status: {}", e) }),
        },
    }
}
