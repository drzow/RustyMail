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

// Each parser function returns `Result<(Result<T, Error>, &str), Error>`:
//
//   * Outer `Err`  — protocol parse failure (incomplete buffer or
//     malformed wire bytes). The caller should request more bytes
//     (`incomplete`) or drop the connection.
//   * `Ok((Err(_), rest))` — wire bytes parsed cleanly but the response
//     was a NO/BYE that classifies into a typed error variant (e.g.
//     ScriptNotFound, Auth). The caller MUST drain `consumed = input.len()
//     - rest.len()` bytes before propagating the error so subsequent
//     commands see a clean buffer.
//   * `Ok((Ok(value), rest))` — happy path.
//
// This split is what lets `client::SieveClient::read_response` drain
// the buffer correctly when a server returns an error response.

/// Parse the response to a CAPABILITY (or post-greeting / post-STARTTLS
/// / post-AUTH) capability listing.
pub fn parse_capabilities(
    input: &str,
) -> Result<(Result<Capabilities, Error>, &str), Error> {
    let (rest, caps, resp) = response_capability(input).map_err(map_parse_err)?;
    let outcome = match classify_response(&resp) {
        Ok(()) => Ok(normalize_capabilities(caps)),
        Err(e) => Err(e),
    };
    Ok((outcome, rest))
}

/// Parse LISTSCRIPTS response. Each entry is `(name, is_active)`.
pub fn parse_listscripts(
    input: &str,
) -> Result<(Result<Vec<(String, bool)>, Error>, &str), Error> {
    let (rest, scripts, resp) = response_listscripts(input).map_err(map_parse_err)?;
    let outcome = match classify_response(&resp) {
        Ok(()) => Ok(scripts),
        Err(e) => Err(e),
    };
    Ok((outcome, rest))
}

/// Parse GETSCRIPT response. NONEXISTENT and friends become
/// `Error::ScriptNotFound` etc. — but the bytes are still consumed.
pub fn parse_getscript(
    input: &str,
) -> Result<(Result<String, Error>, &str), Error> {
    let (rest, body, resp) = response_getscript(input).map_err(map_parse_err)?;
    let outcome = match classify_response(&resp) {
        Ok(()) => body.ok_or_else(|| {
            Error::Protocol("GETSCRIPT OK without script body".into())
        }),
        Err(e) => Err(e),
    };
    Ok((outcome, rest))
}

/// Parse a generic OK/NO/BYE response. Used for every command without
/// a payload (PUTSCRIPT, CHECKSCRIPT, SETACTIVE, DELETESCRIPT,
/// RENAMESCRIPT, NOOP, HAVESPACE, LOGOUT).
pub fn parse_oknobye(input: &str) -> Result<(Result<(), Error>, &str), Error> {
    let (rest, resp) = response_logout(input).map_err(map_parse_err)?;
    Ok((classify_response(&resp), rest))
}

/// Parse AUTHENTICATE response. Returns optional inline caps on
/// success; bare NO maps to `Error::Auth`.
pub fn parse_authenticate_complete(
    input: &str,
) -> Result<(Result<Option<Capabilities>, Error>, &str), Error> {
    let (rest, caps, resp) = response_authenticate_complete(input).map_err(map_parse_err)?;
    let outcome = match classify_response(&resp) {
        Ok(()) => Ok(caps.map(normalize_capabilities)),
        // Bare NO from AUTHENTICATE means auth refused; the parser-level
        // classifier returns `Other` for code-less NO, but in this
        // context that's an auth failure.
        Err(Error::Other(msg)) => Err(Error::Auth(msg)),
        Err(e) => Err(e),
    };
    Ok((outcome, rest))
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
        let (outcome, rest) = parse_oknobye("OK\r\n").unwrap();
        assert!(outcome.is_ok());
        assert_eq!(rest, "");
    }

    #[test]
    fn oknobye_ok_leaves_trailing_input() {
        let (outcome, rest) = parse_oknobye("OK\r\nleftover").unwrap();
        assert!(outcome.is_ok());
        assert_eq!(rest, "leftover");
    }

    #[test]
    fn oknobye_no_with_nonexistent_maps_to_script_not_found() {
        let wire = "NO (NONEXISTENT) \"no such script\"\r\n";
        let (outcome, rest) = parse_oknobye(wire).unwrap();
        // Important: rest must be empty even on classification error —
        // that's what lets the caller drain the consumed bytes.
        assert_eq!(rest, "");
        match outcome {
            Err(Error::ScriptNotFound(msg)) => assert_eq!(msg, "no such script"),
            other => panic!("expected ScriptNotFound, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_bare_no_is_other() {
        let (outcome, rest) = parse_oknobye("NO\r\n").unwrap();
        assert_eq!(rest, "");
        match outcome {
            Err(Error::Other(_)) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_bye_is_disconnected() {
        let (outcome, rest) = parse_oknobye("BYE\r\n").unwrap();
        assert_eq!(rest, "");
        match outcome {
            Err(Error::Disconnected(_)) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn oknobye_truncated_is_protocol_error() {
        match parse_oknobye("OK") {
            Err(Error::Protocol(_)) => {}
            other => panic!("expected outer Protocol error, got {other:?}"),
        }
    }

    // ---------- parse_listscripts ----------

    #[test]
    fn listscripts_empty_returns_empty_vec() {
        let (outcome, rest) = parse_listscripts("OK\r\n").unwrap();
        let scripts = outcome.unwrap();
        assert!(scripts.is_empty());
        assert_eq!(rest, "");
    }

    #[test]
    fn listscripts_marks_active_script() {
        let wire = "\"summer\"\r\n\"vacation\"\r\n\"main\" ACTIVE\r\nOK\r\n";
        let (outcome, _rest) = parse_listscripts(wire).unwrap();
        assert_eq!(
            outcome.unwrap(),
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
        let (outcome, _) = parse_listscripts(wire).unwrap();
        assert_eq!(outcome.unwrap(), vec![("main".to_string(), true)]);
    }

    // ---------- parse_getscript ----------

    #[test]
    fn getscript_returns_literal_body() {
        // Body = "keep;\r\n" = 7 bytes; wire ends with response_ok.
        let wire = "{7}\r\nkeep;\r\n\r\nOK\r\n";
        let (outcome, rest) = parse_getscript(wire).unwrap();
        assert_eq!(outcome.unwrap(), "keep;\r\n");
        assert_eq!(rest, "");
    }

    #[test]
    fn getscript_nonexistent_maps_to_script_not_found() {
        let wire = "NO (NONEXISTENT) \"missing\"\r\n";
        let (outcome, rest) = parse_getscript(wire).unwrap();
        assert_eq!(rest, "", "consumed bytes must be reflected in rest");
        match outcome {
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
        let (outcome, rest) = parse_capabilities(wire).unwrap();
        let caps = outcome.unwrap();
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
        assert_eq!(caps.language.as_deref(), Some("en"));
        assert_eq!(caps.owner.as_deref(), Some("alice@example.com"));
        assert!(caps.unknown.is_empty());
    }

    #[test]
    fn capabilities_minimal_leaves_optionals_unset() {
        let wire = "\"IMPLEMENTATION\" \"tiny\"\r\nOK\r\n";
        let (outcome, _) = parse_capabilities(wire).unwrap();
        let caps = outcome.unwrap();
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
        match parse_authenticate_complete("OK\r\n") {
            Err(Error::Protocol(msg)) => assert!(msg.contains("incomplete")),
            other => panic!("expected outer incomplete, got {other:?}"),
        }
    }

    #[test]
    fn auth_ok_with_capabilities_returns_normalized_caps() {
        let wire =
            "OK\r\n\"IMPLEMENTATION\" \"new-impl\"\r\n\"SIEVE\" \"fileinto\"\r\nOK\r\n";
        let (outcome, rest) = parse_authenticate_complete(wire).unwrap();
        assert_eq!(rest, "");
        let caps = outcome.unwrap().expect("expected new caps after AUTH OK");
        assert_eq!(caps.implementation.as_deref(), Some("new-impl"));
        assert_eq!(caps.sieve_extensions, vec!["fileinto"]);
    }

    #[test]
    fn auth_no_with_sasl_code_maps_to_auth_error() {
        let wire = "NO (SASL) \"bad credentials\"\r\n";
        let (outcome, rest) = parse_authenticate_complete(wire).unwrap();
        assert_eq!(rest, "");
        match outcome {
            Err(Error::Auth(msg)) => assert_eq!(msg, "bad credentials"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn auth_bare_no_maps_to_auth_error() {
        let (outcome, rest) = parse_authenticate_complete("NO\r\n").unwrap();
        assert_eq!(rest, "");
        match outcome {
            Err(Error::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn auth_bye_maps_to_disconnected() {
        let (outcome, rest) = parse_authenticate_complete("BYE\r\n").unwrap();
        assert_eq!(rest, "");
        match outcome {
            Err(Error::Disconnected(_)) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_unknown_capabilities_bucket() {
        let wire = "\"IMPLEMENTATION\" \"tiny\"\r\n\"X-FROBNICATE\" \"yes\"\r\n\"X-BARE\"\r\nOK\r\n";
        let (outcome, _) = parse_capabilities(wire).unwrap();
        assert_eq!(
            outcome.unwrap().unknown,
            vec![
                ("X-FROBNICATE".to_string(), Some("yes".to_string())),
                ("X-BARE".to_string(), None),
            ]
        );
    }
}
