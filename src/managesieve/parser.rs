// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Parser wrappers around the vendored `managesieve` crate.
//!
//! Each function consumes one logical server response off the front of
//! `input` and returns either an `Error` (for protocol/transport
//! problems and tagged failures) or `(payload, remaining_input)`.
//!
//! Callers that read from a streaming socket can keep calling these
//! against an accumulating buffer until they get `Error::Protocol(...)
//! incomplete response`, then read more bytes and retry.

use managesieve::{
    response_authenticate_complete, response_capability, response_getscript,
    response_listscripts, response_logout, Capability, Error as MsError,
};

use super::error::{classify_response, Error};
use super::types::Capabilities;

fn map_parse_err(e: MsError) -> Error {
    match e {
        MsError::IncompleteResponse => Error::Protocol("incomplete response".into()),
        MsError::InvalidResponse => Error::Protocol("invalid response".into()),
        MsError::InvalidInput => Error::Protocol("invalid input".into()),
    }
}

/// Parse the response to a CAPABILITY (or post-greeting / post-STARTTLS
/// / post-AUTH) capability listing, returning a normalized
/// [`Capabilities`] view and any unconsumed input.
pub fn parse_capabilities(input: &str) -> Result<(Capabilities, &str), Error> {
    let (rest, caps, resp) = response_capability(input).map_err(map_parse_err)?;
    classify_response(&resp)?;
    Ok((normalize_capabilities(caps), rest))
}

/// Parse the response to LISTSCRIPTS, returning `(name, is_active)`
/// pairs and any unconsumed input. Script bodies are not part of this
/// reply — fetch them with GETSCRIPT.
pub fn parse_listscripts(input: &str) -> Result<(Vec<(String, bool)>, &str), Error> {
    let (rest, scripts, resp) = response_listscripts(input).map_err(map_parse_err)?;
    classify_response(&resp)?;
    Ok((scripts, rest))
}

/// Parse the response to GETSCRIPT, returning the script body and any
/// unconsumed input. A `NO` response (e.g. NONEXISTENT) maps to a typed
/// [`Error`] rather than producing a body.
pub fn parse_getscript(input: &str) -> Result<(String, &str), Error> {
    let (rest, body, resp) = response_getscript(input).map_err(map_parse_err)?;
    classify_response(&resp)?;
    // classify_response returned Ok, so the response must be OK and the
    // server must have included a body. If not, the server violated the
    // protocol.
    let body = body.ok_or_else(|| {
        Error::Protocol("GETSCRIPT OK without script body".into())
    })?;
    Ok((body, rest))
}

/// Parse a generic OK/NO/BYE response, returning unconsumed input on
/// success. Used for PUTSCRIPT, CHECKSCRIPT, SETACTIVE, DELETESCRIPT,
/// RENAMESCRIPT, NOOP, HAVESPACE, LOGOUT — every command that returns
/// no payload.
pub fn parse_oknobye(input: &str) -> Result<&str, Error> {
    let (rest, resp) = response_logout(input).map_err(map_parse_err)?;
    classify_response(&resp)?;
    Ok(rest)
}

/// Parse the server's response to AUTHENTICATE. Returns the new
/// capabilities (if the server included them after auth) and any
/// unconsumed input. On NO/BYE, returns a typed [`Error::Auth`] /
/// [`Error::Disconnected`].
pub fn parse_authenticate_complete(
    input: &str,
) -> Result<(Option<Capabilities>, &str), Error> {
    let (rest, caps, resp) = response_authenticate_complete(input).map_err(map_parse_err)?;
    if let Err(Error::Other(msg)) = classify_response(&resp) {
        // Bare NO from AUTHENTICATE means auth refused; surface as Auth.
        return Err(Error::Auth(msg));
    }
    classify_response(&resp)?;
    Ok((caps.map(normalize_capabilities), rest))
}

fn normalize_capabilities(items: Vec<Capability>) -> Capabilities {
    let mut out = Capabilities::default();
    for cap in items {
        match cap {
            Capability::Implementation(s) => out.implementation = Some(s),
            Capability::Version(s) => out.version = Some(s),
            Capability::Sieve(v) => out.sieve_extensions = v,
            Capability::Sasl(v) => out.sasl_mechanisms = v,
            Capability::Notify(v) => out.notify_methods = v,
            Capability::MaxRedirects(n) => out.max_redirects = Some(n),
            Capability::StartTls => out.starttls = true,
            Capability::Owner(s) => out.owner = Some(s),
            Capability::Language(s) => out.language = Some(s),
            Capability::Unknown(name, val) => out.unknown.push((name, val)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managesieve::error::Error;

    // ---------- parse_oknobye ----------

    #[test]
    fn oknobye_ok_consumes_response() {
        assert_eq!(parse_oknobye("OK\r\n").unwrap(), "");
    }

    #[test]
    fn oknobye_ok_leaves_trailing_input() {
        assert_eq!(parse_oknobye("OK\r\nleftover").unwrap(), "leftover");
    }

    #[test]
    fn oknobye_no_with_nonexistent_maps_to_script_not_found() {
        let wire = "NO (NONEXISTENT) \"no such script\"\r\n";
        match parse_oknobye(wire) {
            Err(Error::ScriptNotFound(msg)) => assert_eq!(msg, "no such script"),
            other => panic!("expected ScriptNotFound, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_bare_no_is_other() {
        match parse_oknobye("NO\r\n") {
            Err(Error::Other(_)) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_bye_is_disconnected() {
        match parse_oknobye("BYE\r\n") {
            Err(Error::Disconnected(_)) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_truncated_is_protocol_error() {
        match parse_oknobye("OK") {
            Err(Error::Protocol(_)) => {}
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    // ---------- parse_listscripts ----------

    #[test]
    fn listscripts_empty_returns_empty_vec() {
        let (scripts, rest) = parse_listscripts("OK\r\n").unwrap();
        assert!(scripts.is_empty());
        assert_eq!(rest, "");
    }

    #[test]
    fn listscripts_marks_active_script() {
        let wire = "\"summer\"\r\n\"vacation\"\r\n\"main\" ACTIVE\r\nOK\r\n";
        let (scripts, _rest) = parse_listscripts(wire).unwrap();
        assert_eq!(
            scripts,
            vec![
                ("summer".to_string(), false),
                ("vacation".to_string(), false),
                ("main".to_string(), true),
            ]
        );
    }

    #[test]
    fn listscripts_active_keyword_is_case_insensitive() {
        let wire = "\"main\" active\r\nOK\r\n";
        let (scripts, _) = parse_listscripts(wire).unwrap();
        assert_eq!(scripts, vec![("main".to_string(), true)]);
    }

    // ---------- parse_getscript ----------

    #[test]
    fn getscript_returns_literal_body() {
        // Body = "keep;\r\n" = 7 bytes; wire ends with response_ok.
        let wire = "{7}\r\nkeep;\r\n\r\nOK\r\n";
        let (body, rest) = parse_getscript(wire).unwrap();
        assert_eq!(body, "keep;\r\n");
        assert_eq!(rest, "");
    }

    #[test]
    fn getscript_nonexistent_maps_to_script_not_found() {
        let wire = "NO (NONEXISTENT) \"missing\"\r\n";
        match parse_getscript(wire) {
            Err(Error::ScriptNotFound(msg)) => assert_eq!(msg, "missing"),
            other => panic!("expected ScriptNotFound, got {other:?}"),
        }
    }

    // ---------- parse_capabilities ----------

    #[test]
    fn capabilities_full_response_normalizes_into_struct() {
        let wire = concat!(
            "\"IMPLEMENTATION\" \"Cyrus timsieved v2.5.10\"\r\n",
            "\"VERSION\" \"1.0\"\r\n",
            "\"SASL\" \"PLAIN LOGIN\"\r\n",
            "\"SIEVE\" \"fileinto vacation imap4flags\"\r\n",
            "\"STARTTLS\"\r\n",
            "\"NOTIFY\" \"xmpp mailto\"\r\n",
            "\"MAXREDIRECTS\" \"5\"\r\n",
            "\"LANGUAGE\" \"en\"\r\n",
            "\"OWNER\" \"alice@example.com\"\r\n",
            "OK\r\n",
        );
        let (caps, rest) = parse_capabilities(wire).unwrap();
        assert_eq!(rest, "");
        assert_eq!(caps.implementation.as_deref(), Some("Cyrus timsieved v2.5.10"));
        assert_eq!(caps.version.as_deref(), Some("1.0"));
        assert_eq!(caps.sasl_mechanisms, vec!["PLAIN", "LOGIN"]);
        assert_eq!(
            caps.sieve_extensions,
            vec!["fileinto", "vacation", "imap4flags"]
        );
        assert!(caps.starttls);
        assert_eq!(caps.notify_methods, vec!["xmpp", "mailto"]);
        assert_eq!(caps.max_redirects, Some(5));
        // Regression test: upstream 0.1.1 mis-routed LANGUAGE -> owner.
        // Our local fix in vendor/managesieve/src/types.rs keeps them split.
        assert_eq!(caps.language.as_deref(), Some("en"));
        assert_eq!(caps.owner.as_deref(), Some("alice@example.com"));
        assert!(caps.unknown.is_empty());
    }

    #[test]
    fn capabilities_minimal_leaves_optionals_unset() {
        let wire = "\"IMPLEMENTATION\" \"tiny\"\r\nOK\r\n";
        let (caps, _) = parse_capabilities(wire).unwrap();
        assert_eq!(caps.implementation.as_deref(), Some("tiny"));
        assert_eq!(caps.version, None);
        assert_eq!(caps.max_redirects, None);
        assert!(!caps.starttls);
        assert!(caps.sasl_mechanisms.is_empty());
        assert!(caps.sieve_extensions.is_empty());
        assert!(caps.notify_methods.is_empty());
        assert_eq!(caps.owner, None);
        assert_eq!(caps.language, None);
    }

    // ---------- parse_authenticate_complete ----------

    #[test]
    fn auth_ok_alone_is_incomplete() {
        // RFC 5804 lets the server reply with just OK (no inline caps).
        // The streaming parser can't tell if more bytes are coming, so it
        // surfaces Incomplete and the client decides via a read timeout.
        match parse_authenticate_complete("OK\r\n") {
            Err(Error::Protocol(msg)) => assert!(msg.contains("incomplete")),
            other => panic!("expected incomplete, got {other:?}"),
        }
    }

    #[test]
    fn auth_ok_with_capabilities_returns_normalized_caps() {
        let wire =
            "OK\r\n\"IMPLEMENTATION\" \"new-impl\"\r\n\"SIEVE\" \"fileinto\"\r\nOK\r\n";
        let (caps, rest) = parse_authenticate_complete(wire).unwrap();
        assert_eq!(rest, "");
        let caps = caps.expect("expected new caps after AUTH OK");
        assert_eq!(caps.implementation.as_deref(), Some("new-impl"));
        assert_eq!(caps.sieve_extensions, vec!["fileinto"]);
    }

    #[test]
    fn auth_no_with_sasl_code_maps_to_auth_error() {
        let wire = "NO (SASL) \"bad credentials\"\r\n";
        match parse_authenticate_complete(wire) {
            Err(Error::Auth(msg)) => assert_eq!(msg, "bad credentials"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn auth_bare_no_maps_to_auth_error() {
        let wire = "NO\r\n";
        match parse_authenticate_complete(wire) {
            Err(Error::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn auth_bye_maps_to_disconnected() {
        match parse_authenticate_complete("BYE\r\n") {
            Err(Error::Disconnected(_)) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_unknown_capabilities_bucket() {
        let wire = "\"IMPLEMENTATION\" \"tiny\"\r\n\"X-FROBNICATE\" \"yes\"\r\n\"X-BARE\"\r\nOK\r\n";
        let (caps, _) = parse_capabilities(wire).unwrap();
        assert_eq!(
            caps.unknown,
            vec![
                ("X-FROBNICATE".to_string(), Some("yes".to_string())),
                ("X-BARE".to_string(), None),
            ]
        );
    }
}
