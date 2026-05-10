// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! MCP-tool wrappers around `SieveClient` operations.
//!
//! Each function takes an already-connected, already-authenticated
//! client (any `AsyncRead + AsyncWrite + Unpin` stream) and returns
//! `serde_json::Value` shaped for an MCP tool result. The MCP adapter
//! (`crate::mcp::adapters::sdk`) is responsible for opening the
//! connection, calling `authenticate_plain`, dispatching to the right
//! function below, then logging out.
//!
//! Keeping connect/auth/logout out of these functions makes them
//! testable against `tokio::io::duplex` fake servers.

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};

use super::client::SieveClient;
use super::connect::{connect_starttls, DEFAULT_TIMEOUT};
use super::credentials::SieveCredentials;
use super::error::Error;

/// `sieve_capabilities` → server's most-recently-loaded capabilities.
/// Implicitly refreshes from the server (issues a CAPABILITY command).
pub async fn capabilities<S>(client: &mut SieveClient<S>) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let caps = client.capability().await?;
    Ok(json!({
        "implementation": caps.implementation,
        "version": caps.version,
        "sasl_mechanisms": caps.sasl_mechanisms,
        "sieve_extensions": caps.sieve_extensions,
        "notify_methods": caps.notify_methods,
        "max_redirects": caps.max_redirects,
        "starttls": caps.starttls,
        "owner": caps.owner,
        "language": caps.language,
        "unknown": caps.unknown,
    }))
}

/// `sieve_list_scripts` → list every script + its active flag.
pub async fn list_scripts<S>(client: &mut SieveClient<S>) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let scripts = client.list_scripts().await?;
    Ok(json!({
        "scripts": scripts
            .into_iter()
            .map(|(name, active)| json!({"name": name, "active": active}))
            .collect::<Vec<_>>(),
    }))
}

/// `sieve_get_script` → fetch a script body by name. NONEXISTENT
/// surfaces as `Error::ScriptNotFound`.
pub async fn get_script<S>(
    client: &mut SieveClient<S>,
    name: &str,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = client.get_script(name).await?;
    Ok(json!({ "name": name, "body": body }))
}

/// `sieve_put_script` → upload or replace a script. The server
/// validates the body before storing; syntax errors come back as
/// `Error::Other`.
pub async fn put_script<S>(
    client: &mut SieveClient<S>,
    name: &str,
    body: &str,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    client.put_script(name, body).await?;
    Ok(json!({ "status": "ok", "name": name }))
}

/// `sieve_check_script` → validate a body without storing it.
pub async fn check_script<S>(
    client: &mut SieveClient<S>,
    body: &str,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    client.check_script(body).await?;
    Ok(json!({ "status": "ok" }))
}

/// `sieve_set_active` → make `name` the active script. Pass `""` to
/// deactivate without picking a replacement.
pub async fn set_active<S>(
    client: &mut SieveClient<S>,
    name: &str,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    client.set_active(name).await?;
    Ok(json!({ "status": "ok", "name": name }))
}

/// `sieve_delete_script` → remove a script. Active scripts must be
/// deactivated first; that case surfaces as `Error::ScriptActive`.
pub async fn delete_script<S>(
    client: &mut SieveClient<S>,
    name: &str,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    client.delete_script(name).await?;
    Ok(json!({ "status": "ok", "name": name }))
}

/// One-shot dispatcher: open a STARTTLS-upgraded session against
/// `creds`, authenticate via SASL PLAIN, route by tool name, log out,
/// return the JSON result. Logout errors are swallowed (best-effort).
///
/// Used by the MCP adapter (`crate::mcp::adapters::sdk`) so sieve tool
/// calls are stateless from the adapter's perspective.
pub async fn execute_sieve_tool(
    creds: &SieveCredentials,
    tool_name: &str,
    args: &Value,
) -> Result<Value, Error> {
    if !creds.use_starttls {
        return Err(Error::Protocol(
            "cleartext sieve auth not supported in v1".into(),
        ));
    }
    let mut client =
        connect_starttls(&creds.host, creds.port, DEFAULT_TIMEOUT).await?;
    client
        .authenticate_plain(&creds.username, &creds.password)
        .await?;

    let result = dispatch_authenticated(&mut client, tool_name, args).await;

    // Best-effort logout — preserve the dispatch result regardless.
    let _ = client.logout().await;
    result
}

async fn dispatch_authenticated<S>(
    client: &mut SieveClient<S>,
    tool_name: &str,
    args: &Value,
) -> Result<Value, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tool_name {
        "sieve_capabilities" => capabilities(client).await,
        "sieve_list_scripts" => list_scripts(client).await,
        "sieve_get_script" => {
            let name = required_str(args, "name")?;
            get_script(client, name).await
        }
        "sieve_put_script" => {
            let name = required_str(args, "name")?;
            let body = required_str(args, "body")?;
            put_script(client, name, body).await
        }
        "sieve_check_script" => {
            let body = required_str(args, "body")?;
            check_script(client, body).await
        }
        "sieve_set_active" => {
            let name = required_str(args, "name")?;
            set_active(client, name).await
        }
        "sieve_delete_script" => {
            let name = required_str(args, "name")?;
            delete_script(client, name).await
        }
        other => Err(Error::Protocol(format!("unknown sieve tool: {other}"))),
    }
}

fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, Error> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Protocol(format!("missing required string argument {field:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    async fn expect_recv(s: &mut DuplexStream, expected: &[u8]) {
        let mut buf = vec![0u8; expected.len()];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            std::str::from_utf8(expected).unwrap(),
        );
    }

    /// Helper: spin up a duplex pipe whose server side has already sent
    /// the greeting. Returns a connected (but not authenticated)
    /// `SieveClient` plus the server side for the test to drive.
    async fn connected_client() -> (SieveClient<DuplexStream>, tokio::task::JoinHandle<DuplexStream>)
    {
        let (server, client_stream) = tokio::io::duplex(8192);
        let server_handle = tokio::spawn(async move {
            let mut s = server;
            s.write_all(
                b"\"IMPLEMENTATION\" \"test\"\r\n\"VERSION\" \"1.0\"\r\nOK\r\n",
            )
            .await
            .unwrap();
            s
        });
        let client = SieveClient::from_stream(client_stream).await.unwrap();
        (client, server_handle)
    }

    #[tokio::test]
    async fn list_scripts_emits_name_active_pairs() {
        let (mut client, server_handle) = connected_client().await;
        let mut server = server_handle.await.unwrap();
        let server_task = tokio::spawn(async move {
            expect_recv(&mut server, b"LISTSCRIPTS\r\n").await;
            server
                .write_all(b"\"summer\"\r\n\"main\" ACTIVE\r\nOK\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let result = list_scripts(&mut client).await.unwrap();
        server_task.await.unwrap();
        assert_eq!(
            result,
            json!({
                "scripts": [
                    {"name": "summer", "active": false},
                    {"name": "main", "active": true},
                ]
            })
        );
    }

    #[tokio::test]
    async fn get_script_returns_name_and_body() {
        let (mut client, server_handle) = connected_client().await;
        let mut server = server_handle.await.unwrap();
        let server_task = tokio::spawn(async move {
            expect_recv(&mut server, b"GETSCRIPT \"main\"\r\n").await;
            // Body = "keep;\r\n" = 7 bytes.
            server
                .write_all(b"{7}\r\nkeep;\r\n\r\nOK\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let result = get_script(&mut client, "main").await.unwrap();
        server_task.await.unwrap();
        assert_eq!(result, json!({"name": "main", "body": "keep;\r\n"}));
    }

    #[tokio::test]
    async fn put_script_returns_ok_status_with_name() {
        let (mut client, server_handle) = connected_client().await;
        let mut server = server_handle.await.unwrap();
        let server_task = tokio::spawn(async move {
            expect_recv(&mut server, b"PUTSCRIPT \"main\" {6+}\r\nkeep;\n\r\n").await;
            server.write_all(b"OK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let result = put_script(&mut client, "main", "keep;\n").await.unwrap();
        server_task.await.unwrap();
        assert_eq!(result, json!({"status": "ok", "name": "main"}));
    }

    #[tokio::test]
    async fn delete_active_script_propagates_typed_error() {
        let (mut client, server_handle) = connected_client().await;
        let mut server = server_handle.await.unwrap();
        let server_task = tokio::spawn(async move {
            expect_recv(&mut server, b"DELETESCRIPT \"main\"\r\n").await;
            server
                .write_all(b"NO (ACTIVE) \"cannot delete active\"\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        match delete_script(&mut client, "main").await {
            Err(Error::ScriptActive(_)) => {}
            Err(other) => panic!("expected ScriptActive, got {other:?}"),
            Ok(v) => panic!("expected error, got {v}"),
        }
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_protocol_error() {
        let (mut client, server_handle) = connected_client().await;
        let _server = server_handle.await.unwrap();
        match dispatch_authenticated(&mut client, "sieve_imaginary", &json!({})).await {
            Err(Error::Protocol(msg)) => assert!(msg.contains("unknown sieve tool")),
            other => panic!("expected unknown-tool Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_missing_required_arg_returns_protocol_error() {
        let (mut client, server_handle) = connected_client().await;
        let _server = server_handle.await.unwrap();
        // sieve_get_script needs "name"; send empty args.
        match dispatch_authenticated(&mut client, "sieve_get_script", &json!({})).await {
            Err(Error::Protocol(msg)) => {
                assert!(msg.contains("name"), "expected msg to mention 'name', got {msg}");
            }
            other => panic!("expected missing-arg Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_put_script_missing_body_arg_returns_protocol_error() {
        let (mut client, server_handle) = connected_client().await;
        let _server = server_handle.await.unwrap();
        match dispatch_authenticated(
            &mut client,
            "sieve_put_script",
            &json!({"name": "main"}),
        )
        .await
        {
            Err(Error::Protocol(msg)) => {
                assert!(msg.contains("body"), "expected msg to mention 'body', got {msg}");
            }
            other => panic!("expected missing-body Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn capabilities_re_reads_from_server() {
        let (mut client, server_handle) = connected_client().await;
        let mut server = server_handle.await.unwrap();
        let server_task = tokio::spawn(async move {
            expect_recv(&mut server, b"CAPABILITY\r\n").await;
            server
                .write_all(
                    b"\"IMPLEMENTATION\" \"refreshed\"\r\n\
                      \"SIEVE\" \"fileinto vacation\"\r\n\
                      \"STARTTLS\"\r\n\
                      OK\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let result = capabilities(&mut client).await.unwrap();
        server_task.await.unwrap();
        assert_eq!(result["implementation"], "refreshed");
        assert_eq!(result["sieve_extensions"], json!(["fileinto", "vacation"]));
        assert_eq!(result["starttls"], true);
    }
}
