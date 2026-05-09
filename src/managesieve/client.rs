// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Async ManageSieve (RFC 5804) client.
//!
//! Generic over any `AsyncRead + AsyncWrite + Unpin` so callers can plug
//! in a plain TCP stream, a TLS stream, or a test duplex pipe. TLS / TCP
//! connect helpers live elsewhere; this file owns only the protocol
//! state machine.
//!
//! ## Lifecycle
//! ```ignore
//! let stream = TcpStream::connect("sieve.example.com:4190").await?;
//! let mut client = SieveClient::from_stream(stream).await?;
//! client.authenticate_plain("alice", "secret").await?;
//! let scripts = client.list_scripts().await?;
//! client.logout().await?;
//! ```
//!
//! ## SASL support
//! v1 ships PLAIN only. The mailbox.org IMAP account that
//! `email-processing-hand` targets advertises PLAIN over TLS, which is
//! enough. Other mechanisms (LOGIN, OAUTHBEARER, SCRAM) are deferred.
//!
//! ## Servers that omit post-AUTH capabilities
//! RFC 5804 §1.6 says a server MAY reply to AUTHENTICATE with new
//! capabilities, and notes most do. Servers that send a bare `OK\r\n`
//! and then go quiet will currently leave `authenticate_plain` parked
//! waiting for more bytes. Cyrus/Dovecot (Pigeonhole) both send
//! capabilities, so this only matters against unusual servers — we'll
//! add a read timeout if it ever bites us.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use managesieve::Command;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::error::Error;
use super::parser::{
    parse_authenticate_complete, parse_capabilities, parse_getscript, parse_listscripts,
    parse_oknobye,
};
use super::types::Capabilities;

/// How many bytes we read off the wire per syscall.
const READ_BUF_SIZE: usize = 4096;

pub struct SieveClient<S> {
    stream: S,
    capabilities: Capabilities,
    /// Accumulated unparsed bytes from the server.
    buffer: Vec<u8>,
}

impl<S> SieveClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a connected stream and read the server's greeting (a
    /// CAPABILITY-style listing terminated by `OK`).
    pub async fn from_stream(stream: S) -> Result<Self, Error> {
        let mut client = Self {
            stream,
            capabilities: Capabilities::default(),
            buffer: Vec::with_capacity(READ_BUF_SIZE),
        };
        client.capabilities = client.read_response(parse_capabilities).await?;
        Ok(client)
    }

    /// Capabilities the server most recently advertised — initially the
    /// greeting, then refreshed after STARTTLS or AUTHENTICATE.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// SASL PLAIN authentication. Sends an empty authzid, the username,
    /// and the password as a single non-synchronizing literal. Updates
    /// `self.capabilities` if the server returns inline caps after OK.
    pub async fn authenticate_plain(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<(), Error> {
        // RFC 4616: PLAIN payload is \0<authzid>\0<authcid>\0<password>.
        // We send empty authzid which means "act as authcid".
        let mut data = Vec::with_capacity(2 + user.len() + password.len());
        data.push(0);
        data.extend_from_slice(user.as_bytes());
        data.push(0);
        data.extend_from_slice(password.as_bytes());
        let b64 = B64.encode(&data);
        let cmd = format!(
            "AUTHENTICATE \"PLAIN\" {{{}+}}\r\n{}\r\n",
            b64.len(),
            b64,
        );
        self.write_bytes(cmd.as_bytes()).await?;

        let maybe_caps = self.read_response(parse_authenticate_complete).await?;
        if let Some(caps) = maybe_caps {
            self.capabilities = caps;
        }
        Ok(())
    }

    /// Re-fetch the server's capability list. Equivalent to the
    /// CAPABILITY command.
    pub async fn capability(&mut self) -> Result<&Capabilities, Error> {
        self.write_command(&Command::capability()).await?;
        self.capabilities = self.read_response(parse_capabilities).await?;
        Ok(&self.capabilities)
    }

    /// LISTSCRIPTS — return `(name, is_active)` pairs. Bodies aren't
    /// included; fetch them with `get_script` if needed.
    pub async fn list_scripts(&mut self) -> Result<Vec<(String, bool)>, Error> {
        self.write_command(&Command::list_scripts()).await?;
        self.read_response(parse_listscripts).await
    }

    /// GETSCRIPT — fetch a script body by name. NONEXISTENT maps to
    /// [`Error::ScriptNotFound`].
    pub async fn get_script(&mut self, name: &str) -> Result<String, Error> {
        let cmd = Command::getscript(name).map_err(invalid_name(name))?;
        self.write_command(&cmd).await?;
        self.read_response(parse_getscript).await
    }

    /// PUTSCRIPT — upload a script. The server validates the body
    /// before storing; syntax errors come back as `Error::Other`.
    pub async fn put_script(&mut self, name: &str, body: &str) -> Result<(), Error> {
        let cmd = Command::put_script(name, body).map_err(invalid_name(name))?;
        self.write_command(&cmd).await?;
        self.read_response(unit(parse_oknobye)).await
    }

    /// CHECKSCRIPT — validate a script body without storing it.
    pub async fn check_script(&mut self, body: &str) -> Result<(), Error> {
        let cmd = Command::checkscript(body)
            .map_err(|_| Error::Protocol("invalid CHECKSCRIPT body".into()))?;
        self.write_command(&cmd).await?;
        self.read_response(unit(parse_oknobye)).await
    }

    /// SETACTIVE — make `name` the active script. Pass an empty string
    /// to deactivate any currently-active script.
    pub async fn set_active(&mut self, name: &str) -> Result<(), Error> {
        let cmd = Command::set_active(name).map_err(invalid_name(name))?;
        self.write_command(&cmd).await?;
        self.read_response(unit(parse_oknobye)).await
    }

    /// SETACTIVE "" — convenience wrapper for deactivating the active
    /// script without picking a replacement.
    pub async fn deactivate(&mut self) -> Result<(), Error> {
        self.set_active("").await
    }

    /// DELETESCRIPT — server returns `Error::ScriptActive` if the named
    /// script is currently active (deactivate it first).
    pub async fn delete_script(&mut self, name: &str) -> Result<(), Error> {
        let cmd = Command::deletescript(name).map_err(invalid_name(name))?;
        self.write_command(&cmd).await?;
        self.read_response(unit(parse_oknobye)).await
    }

    /// NOOP — round-trip ping.
    pub async fn noop(&mut self) -> Result<(), Error> {
        self.write_command(&Command::noop()).await?;
        self.read_response(unit(parse_oknobye)).await
    }

    /// LOGOUT — send LOGOUT, await OK, drop the stream.
    pub async fn logout(mut self) -> Result<(), Error> {
        self.write_command(&Command::logout()).await?;
        self.read_response(unit(parse_oknobye)).await?;
        Ok(())
    }

    // ---------- internal helpers ----------

    async fn write_command(&mut self, cmd: &Command) -> Result<(), Error> {
        self.write_bytes(cmd.to_string().as_bytes()).await
    }

    async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|e| Error::Connection(format!("write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| Error::Connection(format!("flush: {e}")))?;
        Ok(())
    }

    /// Drive `parser` against the current buffer, reading more bytes
    /// from the stream whenever it returns `Error::Protocol("incomplete
    /// response")`. Drains consumed bytes from the buffer on success.
    async fn read_response<P, T>(&mut self, parser: P) -> Result<T, Error>
    where
        P: Fn(&str) -> Result<(T, &str), Error>,
    {
        loop {
            // Try the parser without holding any borrows of self.buffer
            // across the await.
            let outcome: Result<Option<(T, usize)>, Error> = {
                let utf8_prefix = match std::str::from_utf8(&self.buffer) {
                    Ok(s) => s,
                    Err(e) => {
                        // Trailing partial UTF-8 sequence — keep what's
                        // valid and read more for the rest.
                        std::str::from_utf8(&self.buffer[..e.valid_up_to()]).unwrap()
                    }
                };
                if utf8_prefix.is_empty() {
                    Ok(None)
                } else {
                    match parser(utf8_prefix) {
                        Ok((value, rest)) => {
                            let consumed = utf8_prefix.len() - rest.len();
                            Ok(Some((value, consumed)))
                        }
                        Err(Error::Protocol(m)) if m.contains("incomplete") => Ok(None),
                        Err(e) => Err(e),
                    }
                }
            };
            match outcome? {
                Some((value, consumed)) => {
                    self.buffer.drain(..consumed);
                    return Ok(value);
                }
                None => self.recv_chunk().await?,
            }
        }
    }

    async fn recv_chunk(&mut self) -> Result<(), Error> {
        let mut buf = [0u8; READ_BUF_SIZE];
        let n = self
            .stream
            .read(&mut buf)
            .await
            .map_err(|e| Error::Connection(format!("read: {e}")))?;
        if n == 0 {
            return Err(Error::Disconnected("server closed connection".into()));
        }
        self.buffer.extend_from_slice(&buf[..n]);
        Ok(())
    }
}

/// Adapter so `parse_oknobye` (which returns `&str` only) fits the
/// `read_response` signature that expects `(T, &str)`.
fn unit<F>(f: F) -> impl Fn(&str) -> Result<((), &str), Error>
where
    F: Fn(&str) -> Result<&str, Error>,
{
    move |s| f(s).map(|rest| ((), rest))
}

fn invalid_name(name: &str) -> impl FnOnce(managesieve::Error) -> Error + '_ {
    move |_| Error::Protocol(format!("invalid sieve script name: {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// Drive a duplex stream as a fake server in a spawned task.
    fn fake_server<F, Fut>(server: DuplexStream, script: F) -> tokio::task::JoinHandle<()>
    where
        F: FnOnce(DuplexStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move { script(server).await })
    }

    /// Read `expected.len()` bytes off the server-side stream and assert
    /// they match.
    async fn expect_recv(s: &mut DuplexStream, expected: &[u8]) {
        let mut buf = vec![0u8; expected.len()];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            std::str::from_utf8(expected).unwrap(),
            "client wrote unexpected bytes",
        );
    }

    #[tokio::test]
    async fn from_stream_parses_greeting_capabilities() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(
                b"\"IMPLEMENTATION\" \"Test Sieved v1.0\"\r\n\
                  \"VERSION\" \"1.0\"\r\n\
                  \"SASL\" \"PLAIN\"\r\n\
                  \"SIEVE\" \"fileinto vacation\"\r\n\
                  \"STARTTLS\"\r\n\
                  OK\r\n",
            )
            .await
            .unwrap();
            // Keep stream open briefly so client sees the bytes.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let client = SieveClient::from_stream(client_stream).await.unwrap();
        assert_eq!(
            client.capabilities().implementation.as_deref(),
            Some("Test Sieved v1.0"),
        );
        assert_eq!(client.capabilities().sasl_mechanisms, vec!["PLAIN"]);
        assert!(client.capabilities().starttls);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn from_stream_surfaces_disconnect_on_eof() {
        let (server, client_stream) = tokio::io::duplex(8192);
        // Server closes immediately without sending greeting.
        drop(server);
        match SieveClient::from_stream(client_stream).await {
            Err(Error::Disconnected(_)) => {}
            Err(other) => panic!("expected Disconnected, got error {other:?}"),
            Ok(_) => panic!("expected Disconnected, got Ok client"),
        }
    }

    #[tokio::test]
    async fn authenticate_plain_writes_correct_wire_bytes() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            // Expected client wire: AUTHENTICATE "PLAIN" {N+}\r\n<b64>\r\n
            // For user "alice", pass "secret":
            //   payload bytes = \0 a l i c e \0 s e c r e t = 13 bytes
            //   base64 = "AGFsaWNlAHNlY3JldA=="  (20 chars)
            expect_recv(
                &mut s,
                b"AUTHENTICATE \"PLAIN\" {20+}\r\nAGFsaWNlAHNlY3JldA==\r\n",
            )
            .await;
            // Reply OK with no inline caps would hang; send inline caps
            // followed by a final OK to exercise the realistic path.
            s.write_all(b"OK\r\n\"IMPLEMENTATION\" \"x\"\r\nOK\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        client.authenticate_plain("alice", "secret").await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn authenticate_plain_no_response_maps_to_auth_error() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            let mut buf = [0u8; 256];
            let _n = s.read(&mut buf).await.unwrap();
            s.write_all(b"NO (SASL) \"bad creds\"\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        match client.authenticate_plain("alice", "wrong").await {
            Err(Error::Auth(msg)) => assert_eq!(msg, "bad creds"),
            other => panic!("expected Auth, got {other:?}"),
        }
        task.await.unwrap();
    }

    #[tokio::test]
    async fn list_scripts_round_trip() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            expect_recv(&mut s, b"LISTSCRIPTS\r\n").await;
            s.write_all(b"\"summer\"\r\n\"main\" ACTIVE\r\nOK\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        let scripts = client.list_scripts().await.unwrap();
        assert_eq!(
            scripts,
            vec![("summer".to_string(), false), ("main".to_string(), true)],
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn get_script_returns_literal_body() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            expect_recv(&mut s, b"GETSCRIPT \"main\"\r\n").await;
            // Body = "keep;\r\n" = 7 bytes; followed by \r\nOK\r\n.
            s.write_all(b"{7}\r\nkeep;\r\n\r\nOK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        let body = client.get_script("main").await.unwrap();
        assert_eq!(body, "keep;\r\n");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn put_script_sends_literal_in_request() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            // PUTSCRIPT body uses non-synchronizing literal: {N+}\r\n<body>
            expect_recv(
                &mut s,
                b"PUTSCRIPT \"main\" {6+}\r\nkeep;\n\r\n",
            )
            .await;
            s.write_all(b"OK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        client.put_script("main", "keep;\n").await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn delete_active_script_returns_script_active_error() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            expect_recv(&mut s, b"DELETESCRIPT \"main\"\r\n").await;
            s.write_all(b"NO (ACTIVE) \"cannot delete active\"\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        match client.delete_script("main").await {
            Err(Error::ScriptActive(msg)) => {
                assert_eq!(msg, "cannot delete active");
            }
            other => panic!("expected ScriptActive, got {other:?}"),
        }
        task.await.unwrap();
    }

    #[tokio::test]
    async fn partial_response_buffers_until_parser_succeeds() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            expect_recv(&mut s, b"NOOP\r\n").await;
            // Send response in three chunks with delays — client must
            // buffer across reads until parser sees the full \r\n.
            s.write_all(b"O").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            s.write_all(b"K\r").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            s.write_all(b"\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut client = SieveClient::from_stream(client_stream).await.unwrap();
        client.noop().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn logout_consumes_self_and_returns_ok() {
        let (server, client_stream) = tokio::io::duplex(8192);
        let task = fake_server(server, |mut s| async move {
            s.write_all(b"\"IMPLEMENTATION\" \"x\"\r\nOK\r\n").await.unwrap();
            expect_recv(&mut s, b"LOGOUT\r\n").await;
            s.write_all(b"OK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let client = SieveClient::from_stream(client_stream).await.unwrap();
        client.logout().await.unwrap();
        task.await.unwrap();
    }
}
