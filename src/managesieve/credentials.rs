// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Sieve-credential resolution from `accounts.json`.
//!
//! By default, sieve credentials are derived from the account's IMAP
//! settings (same host, default port 4190, same username + password,
//! STARTTLS upgrade). An account can override this by adding a
//! top-level `"sieve"` block alongside `"imap"` and `"smtp"`.
//!
//! ```json
//! {
//!   "version": "1.0",
//!   "default_account_id": "alice@mailbox.org",
//!   "accounts": [
//!     {
//!       "display_name": "Alice",
//!       "email_address": "alice@mailbox.org",
//!       "imap": { "host": "imap.mailbox.org", "port": 993, ... },
//!       "sieve": { "host": "sieve.mailbox.org", "port": 4190, ... }
//!     }
//!   ]
//! }
//! ```
//!
//! This module deliberately reads the JSON directly with a minimal
//! serde shape rather than reusing `dashboard::services::Account` —
//! the sieve module shouldn't depend on the dashboard layer.

use std::path::Path;

use serde::Deserialize;

use super::error::Error;

/// Credentials needed to open a SASL-PLAIN-authenticated ManageSieve
/// connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SieveCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// True when the connection should use STARTTLS upgrade. False
    /// means cleartext (only honored for plaintext-only test servers).
    pub use_starttls: bool,
}

/// Look up sieve credentials for the named account in `accounts.json`,
/// applying the `imap`-fallback rule when no `sieve` block is set.
///
/// `account_id` of `None` resolves to the file's `default_account_id`.
pub fn resolve_for_account(
    accounts_path: impl AsRef<Path>,
    account_id: Option<&str>,
) -> Result<SieveCredentials, Error> {
    let raw = std::fs::read_to_string(accounts_path.as_ref()).map_err(|e| {
        Error::Connection(format!(
            "read {}: {e}",
            accounts_path.as_ref().display()
        ))
    })?;
    let store: AccountsFile = serde_json::from_str(&raw)
        .map_err(|e| Error::Protocol(format!("parse accounts.json: {e}")))?;
    resolve_from_store(&store, account_id)
}

fn resolve_from_store(
    store: &AccountsFile,
    account_id: Option<&str>,
) -> Result<SieveCredentials, Error> {
    let target_id = account_id.or(store.default_account_id.as_deref()).ok_or_else(
        || Error::Connection("no account_id given and no default_account_id in accounts.json".into()),
    )?;
    let account = store
        .accounts
        .iter()
        .find(|a| a.email_address == target_id)
        .ok_or_else(|| {
            Error::Connection(format!("account {target_id:?} not in accounts.json"))
        })?;
    Ok(merge_sieve_with_imap_fallback(account))
}

fn merge_sieve_with_imap_fallback(account: &AccountEntry) -> SieveCredentials {
    if let Some(sieve) = &account.sieve {
        SieveCredentials {
            host: sieve.host.clone(),
            port: sieve.port,
            username: sieve.username.clone(),
            password: sieve.password.clone(),
            use_starttls: sieve.use_starttls.unwrap_or(true),
        }
    } else {
        SieveCredentials {
            host: account.imap.host.clone(),
            port: super::connect::DEFAULT_PORT,
            username: account.imap.username.clone(),
            password: account.imap.password.clone(),
            use_starttls: true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    default_account_id: Option<String>,
    accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    email_address: String,
    imap: ImapBlock,
    #[serde(default)]
    sieve: Option<SieveBlock>,
}

#[derive(Debug, Deserialize)]
struct ImapBlock {
    host: String,
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SieveBlock {
    host: String,
    port: u16,
    username: String,
    password: String,
    #[serde(default)]
    use_starttls: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_accounts_json(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn falls_back_to_imap_credentials_when_sieve_block_missing() {
        let f = write_accounts_json(
            r#"{
              "version": "1.0",
              "default_account_id": "alice@example.com",
              "accounts": [
                {
                  "email_address": "alice@example.com",
                  "imap": {
                    "host": "imap.example.com",
                    "port": 993,
                    "username": "alice@example.com",
                    "password": "secret",
                    "use_tls": true
                  }
                }
              ]
            }"#,
        );
        let creds = resolve_for_account(f.path(), None).unwrap();
        assert_eq!(
            creds,
            SieveCredentials {
                host: "imap.example.com".into(),
                port: super::super::connect::DEFAULT_PORT,
                username: "alice@example.com".into(),
                password: "secret".into(),
                use_starttls: true,
            }
        );
    }

    #[test]
    fn sieve_block_overrides_imap_defaults() {
        let f = write_accounts_json(
            r#"{
              "version": "1.0",
              "default_account_id": "alice@mailbox.org",
              "accounts": [
                {
                  "email_address": "alice@mailbox.org",
                  "imap": {
                    "host": "imap.mailbox.org",
                    "port": 993,
                    "username": "alice@mailbox.org",
                    "password": "imap-pass",
                    "use_tls": true
                  },
                  "sieve": {
                    "host": "sieve.mailbox.org",
                    "port": 4190,
                    "username": "alice",
                    "password": "sieve-app-password",
                    "use_starttls": true
                  }
                }
              ]
            }"#,
        );
        let creds = resolve_for_account(f.path(), None).unwrap();
        assert_eq!(creds.host, "sieve.mailbox.org");
        assert_eq!(creds.port, 4190);
        assert_eq!(creds.username, "alice"); // not the IMAP username
        assert_eq!(creds.password, "sieve-app-password");
        assert!(creds.use_starttls);
    }

    #[test]
    fn explicit_account_id_overrides_default() {
        let f = write_accounts_json(
            r#"{
              "version": "1.0",
              "default_account_id": "alice@example.com",
              "accounts": [
                {
                  "email_address": "alice@example.com",
                  "imap": {
                    "host": "imap1", "port": 993, "username": "alice",
                    "password": "p1", "use_tls": true
                  }
                },
                {
                  "email_address": "bob@example.com",
                  "imap": {
                    "host": "imap2", "port": 993, "username": "bob",
                    "password": "p2", "use_tls": true
                  }
                }
              ]
            }"#,
        );
        let creds = resolve_for_account(f.path(), Some("bob@example.com")).unwrap();
        assert_eq!(creds.username, "bob");
        assert_eq!(creds.host, "imap2");
    }

    #[test]
    fn missing_account_returns_connection_error() {
        let f = write_accounts_json(
            r#"{
              "default_account_id": "alice@example.com",
              "accounts": [
                {
                  "email_address": "alice@example.com",
                  "imap": {
                    "host": "imap1", "port": 993, "username": "alice",
                    "password": "p1", "use_tls": true
                  }
                }
              ]
            }"#,
        );
        match resolve_for_account(f.path(), Some("ghost@example.com")) {
            Err(Error::Connection(msg)) => {
                assert!(msg.contains("ghost@example.com"));
            }
            other => panic!("expected Connection error, got {other:?}"),
        }
    }

    #[test]
    fn no_default_and_no_explicit_id_is_an_error() {
        let f = write_accounts_json(
            r#"{
              "accounts": [
                {
                  "email_address": "alice@example.com",
                  "imap": {
                    "host": "imap1", "port": 993, "username": "alice",
                    "password": "p1", "use_tls": true
                  }
                }
              ]
            }"#,
        );
        match resolve_for_account(f.path(), None) {
            Err(Error::Connection(msg)) => assert!(msg.contains("default_account_id")),
            other => panic!("expected Connection error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_surfaces_as_protocol_error() {
        let f = write_accounts_json("{ this is not json");
        match resolve_for_account(f.path(), None) {
            Err(Error::Protocol(msg)) => assert!(msg.contains("accounts.json")),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }
}
