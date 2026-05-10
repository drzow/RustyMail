// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Live integration test for the ManageSieve client.
//!
//! Compiled only under `--features integration-sieve`. Expects a
//! Pigeonhole-enabled Dovecot listening on `127.0.0.1:4190` with a
//! `testuser` / `testpass` account. See
//! `tests/fixtures/managesieve/README.md` for the docker-compose fixture.
//!
//! Run:
//! ```bash
//! docker compose -f tests/fixtures/managesieve/docker-compose.yml up -d --build
//! cargo test --features integration-sieve --test integration_sieve -- --nocapture
//! ```

#![cfg(feature = "integration-sieve")]

use std::time::Duration;

use rustymail::managesieve::{connect::connect_starttls_insecure, Error};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 4190;
const USER: &str = "testuser";
const PASS: &str = "testpass";
const TEST_SCRIPT_NAME: &str = "rustymail_test_script";
const TEST_SCRIPT_BODY: &str = "require [\"fileinto\"];\nfileinto \"INBOX\";\n";

fn timeout() -> Duration {
    Duration::from_secs(10)
}

/// If the fixture isn't running, skip with a clear message rather
/// than hard-failing. Lets devs run `cargo test --all-features`
/// without first booting Docker.
async fn connect_or_skip() -> Option<rustymail::managesieve::SieveClient<tokio_native_tls::TlsStream<tokio::net::TcpStream>>>
{
    match connect_starttls_insecure(HOST, PORT, timeout()).await {
        Ok(client) => Some(client),
        Err(Error::Connection(msg)) => {
            eprintln!(
                "SKIP: cannot reach Pigeonhole fixture at {HOST}:{PORT} — {msg}\n\
                 Run: docker compose -f tests/fixtures/managesieve/docker-compose.yml up -d --build"
            );
            None
        }
        Err(other) => panic!("unexpected error connecting to fixture: {other:?}"),
    }
}

#[tokio::test]
async fn full_script_lifecycle_round_trip() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };

    // Server should advertise the extensions the test exercises.
    let caps = client.capabilities();
    assert!(
        caps.sieve_extensions.iter().any(|s| s == "fileinto"),
        "fixture should support fileinto; got {:?}",
        caps.sieve_extensions,
    );

    // Authenticate.
    client.authenticate_plain(USER, PASS).await.expect("PLAIN auth");

    // Clean up any leftover from a previous failed run, best-effort.
    let _ = client.deactivate().await;
    let _ = client.delete_script(TEST_SCRIPT_NAME).await;

    // Validate-only first — script must compile.
    client
        .check_script(TEST_SCRIPT_BODY)
        .await
        .expect("CHECKSCRIPT on a valid body");

    // PUT.
    client
        .put_script(TEST_SCRIPT_NAME, TEST_SCRIPT_BODY)
        .await
        .expect("PUTSCRIPT");

    // LIST — script appears, not yet active.
    let scripts = client.list_scripts().await.expect("LISTSCRIPTS");
    let row = scripts
        .iter()
        .find(|(n, _)| n == TEST_SCRIPT_NAME)
        .expect("uploaded script should appear in LISTSCRIPTS");
    assert!(!row.1, "script should be inactive immediately after PUT");

    // GET — body round-trips.
    let fetched = client
        .get_script(TEST_SCRIPT_NAME)
        .await
        .expect("GETSCRIPT");
    assert_eq!(
        fetched, TEST_SCRIPT_BODY,
        "body fetched via GETSCRIPT must match what we PUT",
    );

    // SET ACTIVE — script is now active.
    client
        .set_active(TEST_SCRIPT_NAME)
        .await
        .expect("SETACTIVE");
    let scripts = client.list_scripts().await.unwrap();
    let active = scripts
        .iter()
        .find(|(_, a)| *a)
        .map(|(n, _)| n.clone());
    assert_eq!(
        active.as_deref(),
        Some(TEST_SCRIPT_NAME),
        "our script should be the active one",
    );

    // DELETE while active — must fail with ScriptActive.
    match client.delete_script(TEST_SCRIPT_NAME).await {
        Err(Error::ScriptActive(_)) => {}
        other => panic!("expected ScriptActive on delete-while-active, got {other:?}"),
    }

    // Deactivate then delete — should succeed.
    client.deactivate().await.expect("SETACTIVE \"\"");
    client
        .delete_script(TEST_SCRIPT_NAME)
        .await
        .expect("DELETESCRIPT after deactivate");

    // LIST — script is gone.
    let scripts = client.list_scripts().await.unwrap();
    assert!(
        !scripts.iter().any(|(n, _)| n == TEST_SCRIPT_NAME),
        "script should be absent after DELETESCRIPT, got {scripts:?}",
    );

    client.logout().await.expect("LOGOUT");
}

#[tokio::test]
async fn nonexistent_script_returns_typed_error() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };
    client.authenticate_plain(USER, PASS).await.unwrap();

    match client.get_script("definitely_does_not_exist_xyzzy").await {
        Err(Error::ScriptNotFound(_)) => {}
        other => panic!("expected ScriptNotFound, got {other:?}"),
    }

    client.logout().await.unwrap();
}

#[tokio::test]
async fn invalid_script_body_is_rejected_by_checkscript() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };
    client.authenticate_plain(USER, PASS).await.unwrap();

    // Missing `require` for fileinto = compile error per Pigeonhole.
    let bad = "fileinto \"INBOX\";\n";
    match client.check_script(bad).await {
        Err(Error::Other(msg)) => {
            // Pigeonhole returns a NO with a free-text message; we
            // surface as Other since there's no specific response code.
            assert!(
                !msg.is_empty(),
                "expected a non-empty error message from server"
            );
        }
        Err(Error::Warnings(_)) => {
            // Some Pigeonhole versions return WARNINGS instead of NO.
            // Either way the body was rejected as we expected.
        }
        other => panic!("expected Other or Warnings on bad script, got {other:?}"),
    }

    client.logout().await.unwrap();
}

#[tokio::test]
async fn auth_with_bad_password_maps_to_typed_error() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };
    match client.authenticate_plain(USER, "wrong-password").await {
        Err(Error::Auth(_)) => {}
        other => panic!("expected Auth error on bad password, got {other:?}"),
    }
}
