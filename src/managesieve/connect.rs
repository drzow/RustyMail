// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Connection helpers — TCP + STARTTLS upgrade.
//!
//! ManageSieve servers in the wild (mailbox.org, Cyrus, Pigeonhole) all
//! listen on port 4190 with cleartext greeting + STARTTLS upgrade.
//! Implicit-TLS Sieve (port 5190) exists but is rare; we don't ship a
//! helper for it yet — callers can build one by skipping the STARTTLS
//! request and going straight to TLS handshake.

use std::time::Duration;

use native_tls::TlsConnector;
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector as TokioTlsConnector, TlsStream};

use super::client::SieveClient;
use super::error::Error;

/// Default ManageSieve port (RFC 5804 §1.4).
pub const DEFAULT_PORT: u16 = 4190;

/// Default connect + handshake timeout. ManageSieve handshakes are
/// quick on healthy servers; longer timeouts here just delay error
/// surfacing when a server is unreachable.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Connect to a ManageSieve server and upgrade the connection via
/// STARTTLS. The returned client has the post-handshake capabilities
/// loaded; the caller is responsible for authentication.
///
/// Errors:
/// - `Error::Connection` — TCP connect failed or the TLS handshake
///   failed.
/// - `Error::Protocol("server does not advertise STARTTLS")` — the
///   greeting capabilities did not include STARTTLS, so we refuse to
///   send credentials over cleartext.
/// - `Error::Disconnected` — server hung up during the handshake.
pub async fn connect_starttls(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<SieveClient<TlsStream<TcpStream>>, Error> {
    // Phase 1: TCP connect.
    let tcp = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Connection(format!("connect to {host}:{port} timed out")))?
        .map_err(|e| Error::Connection(format!("connect to {host}:{port}: {e}")))?;
    tcp.set_nodelay(true)
        .map_err(|e| Error::Connection(format!("set TCP_NODELAY: {e}")))?;

    // Phase 2: greeting + STARTTLS request, on the cleartext socket.
    let mut client = SieveClient::from_stream(tcp).await?;
    if !client.capabilities().starttls {
        return Err(Error::Protocol(
            "server does not advertise STARTTLS; refusing cleartext auth".into(),
        ));
    }
    client.starttls_request().await?;
    let tcp = client.into_inner()?;

    // Phase 3: TLS handshake. native_tls picks up the OS root CA store
    // automatically, matching how the IMAP client does it.
    let native_connector = TlsConnector::builder()
        .build()
        .map_err(|e| Error::Connection(format!("build TLS connector: {e}")))?;
    let connector = TokioTlsConnector::from(native_connector);
    let tls_stream = tokio::time::timeout(timeout, connector.connect(host, tcp))
        .await
        .map_err(|_| Error::Connection("TLS handshake timed out".into()))?
        .map_err(|e| Error::Connection(format!("TLS handshake: {e}")))?;

    // Phase 4: fresh SieveClient on the TLS stream re-reads the
    // post-handshake capability listing per RFC 5804 §2.2.
    SieveClient::from_stream(tls_stream).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Drive a TCP listener that pretends to be a Sieve server but
    /// refuses STARTTLS by omitting it from the greeting capabilities.
    /// We expect connect_starttls to bail before any TLS handshake.
    #[tokio::test]
    async fn refuses_to_authenticate_cleartext_when_starttls_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Greeting WITHOUT a STARTTLS capability line.
            sock.write_all(b"\"IMPLEMENTATION\" \"insecure-server\"\r\nOK\r\n")
                .await
                .unwrap();
            // Drain whatever the client sends before bailing.
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf).await;
        });

        match connect_starttls("127.0.0.1", port, Duration::from_secs(2)).await {
            Err(Error::Protocol(msg)) => {
                assert!(
                    msg.contains("STARTTLS"),
                    "expected STARTTLS-related protocol error, got: {msg}",
                );
            }
            Err(other) => panic!("expected Protocol(STARTTLS), got error {other:?}"),
            Ok(_) => panic!("expected Protocol(STARTTLS), got an authenticated client"),
        }
    }

    /// connect_starttls with a refused TCP connect should surface as
    /// Error::Connection, not bubble the io::Error verbatim.
    #[tokio::test]
    async fn connect_failure_maps_to_connection_error() {
        // Bind and immediately drop to free the port — connecting to it
        // (likely) fails with ECONNREFUSED.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        match connect_starttls("127.0.0.1", port, Duration::from_secs(1)).await {
            Err(Error::Connection(_)) => {}
            Err(other) => panic!("expected Connection error, got {other:?}"),
            Ok(_) => panic!("expected Connection error, got an authenticated client"),
        }
    }
}
