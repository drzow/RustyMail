// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Error types for ManageSieve operations.
//!
//! `Error` is the user-facing surface; `classify_response()` maps an
//! underlying `managesieve::Response` (OK/NO/BYE + RFC 5804 §1.3 response
//! code) into one of our variants so callers can pattern-match on
//! semantics rather than parsing free-text human messages.

use thiserror::Error;

use managesieve::{OkNoBye, QuotaVariant, Response, ResponseCode};

#[derive(Debug, Clone, PartialEq, Error)]
pub enum Error {
    /// TCP / TLS / I/O failures.
    #[error("connection error: {0}")]
    Connection(String),

    /// Server response could not be parsed or violated the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// SASL / authentication failures (AuthTooWeak, EncryptNeeded, Sasl,
    /// TransitionNeeded, or a NO with no specific code from AUTHENTICATE).
    #[error("authentication error: {0}")]
    Auth(String),

    /// `Nonexistent` — the named script is not on the server.
    #[error("script not found: {0}")]
    ScriptNotFound(String),

    /// `AlreadyExists` — a PUTSCRIPT/RENAMESCRIPT collided with an existing
    /// script and the server refused to overwrite.
    #[error("script already exists: {0}")]
    ScriptAlreadyExists(String),

    /// `Active` — DELETESCRIPT refused because the script is currently
    /// active (must SETACTIVE "" first).
    #[error("script is active and cannot be deleted: {0}")]
    ScriptActive(String),

    /// `Quota(...)` — over MaxScripts, MaxSize, or generic.
    #[error("quota exceeded ({kind:?}): {message}")]
    Quota {
        kind: QuotaVariant,
        message: String,
    },

    /// `TryLater` — transient server-side back-pressure; safe to retry.
    #[error("server asked us to try later: {0}")]
    TryLater(String),

    /// `Referral(url)` — server redirects us to another ManageSieve URL.
    #[error("server referral to {url}: {message}")]
    Referral { url: String, message: String },

    /// `Warnings` — the script was accepted (CHECKSCRIPT or PUTSCRIPT) but
    /// the server attached non-fatal warnings. Returned only when the
    /// caller asks for warning surfacing; otherwise treated as success.
    #[error("script accepted with warnings: {0}")]
    Warnings(String),

    /// Server sent BYE; connection has dropped.
    #[error("server disconnected: {0}")]
    Disconnected(String),

    /// Catch-all for codes we don't promote (e.g. `Tag`) and untagged NOs.
    #[error("server rejected request: {0}")]
    Other(String),
}

/// Map a `managesieve::Response` into an Error when it represents a
/// failure, or `Ok(())` when it represents success.
///
/// `Warnings` is treated as success (the script was accepted); callers
/// that need to surface warnings should inspect the response separately.
pub fn classify_response(resp: &Response) -> Result<(), Error> {
    let human = resp
        .human
        .clone()
        .unwrap_or_else(|| match resp.tag {
            OkNoBye::Ok => "OK".into(),
            OkNoBye::No => "NO".into(),
            OkNoBye::Bye => "BYE".into(),
        });

    match resp.tag {
        OkNoBye::Ok => Ok(()),
        OkNoBye::Bye => Err(Error::Disconnected(human)),
        OkNoBye::No => Err(match &resp.code {
            None => Error::Other(human),
            Some((code, _arg)) => match code {
                ResponseCode::Nonexistent => Error::ScriptNotFound(human),
                ResponseCode::AlreadyExists => Error::ScriptAlreadyExists(human),
                ResponseCode::Active => Error::ScriptActive(human),
                ResponseCode::AuthTooWeak
                | ResponseCode::EncryptNeeded
                | ResponseCode::Sasl
                | ResponseCode::TransitionNeeded => Error::Auth(human),
                ResponseCode::Quota(kind) => Error::Quota {
                    kind: *kind,
                    message: human,
                },
                ResponseCode::TryLater => Error::TryLater(human),
                ResponseCode::Referral(url) => Error::Referral {
                    url: url.clone(),
                    message: human,
                },
                ResponseCode::Warnings => Error::Warnings(human),
                ResponseCode::Tag => Error::Other(human),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_with(code: ResponseCode, human: &str) -> Response {
        Response {
            tag: OkNoBye::No,
            code: Some((code, None)),
            human: Some(human.into()),
        }
    }

    #[test]
    fn ok_response_is_success() {
        let resp = Response {
            tag: OkNoBye::Ok,
            code: None,
            human: Some("Done.".into()),
        };
        assert_eq!(classify_response(&resp), Ok(()));
    }

    #[test]
    fn nonexistent_maps_to_script_not_found() {
        let resp = no_with(ResponseCode::Nonexistent, "no such script");
        assert_eq!(
            classify_response(&resp),
            Err(Error::ScriptNotFound("no such script".into()))
        );
    }

    #[test]
    fn already_exists_maps_to_script_already_exists() {
        let resp = no_with(ResponseCode::AlreadyExists, "script already exists");
        assert_eq!(
            classify_response(&resp),
            Err(Error::ScriptAlreadyExists("script already exists".into()))
        );
    }

    #[test]
    fn active_maps_to_script_active() {
        let resp = no_with(ResponseCode::Active, "cannot delete active script");
        assert_eq!(
            classify_response(&resp),
            Err(Error::ScriptActive("cannot delete active script".into()))
        );
    }

    #[test]
    fn auth_codes_map_to_auth() {
        for code in [
            ResponseCode::AuthTooWeak,
            ResponseCode::EncryptNeeded,
            ResponseCode::Sasl,
            ResponseCode::TransitionNeeded,
        ] {
            let resp = no_with(code, "auth refused");
            assert!(
                matches!(classify_response(&resp), Err(Error::Auth(_))),
                "{:?} should map to Error::Auth",
                resp.code
            );
        }
    }

    #[test]
    fn quota_variants_carry_kind() {
        let resp = no_with(
            ResponseCode::Quota(QuotaVariant::MaxScripts),
            "too many scripts",
        );
        match classify_response(&resp) {
            Err(Error::Quota { kind, message }) => {
                assert_eq!(kind, QuotaVariant::MaxScripts);
                assert_eq!(message, "too many scripts");
            }
            other => panic!("expected Quota, got {other:?}"),
        }
    }

    #[test]
    fn try_later_maps_to_try_later() {
        let resp = no_with(ResponseCode::TryLater, "busy, retry");
        assert_eq!(
            classify_response(&resp),
            Err(Error::TryLater("busy, retry".into()))
        );
    }

    #[test]
    fn referral_carries_url() {
        let url = "sieve://elsewhere.example/".to_string();
        let resp = no_with(ResponseCode::Referral(url.clone()), "moved");
        match classify_response(&resp) {
            Err(Error::Referral { url: got_url, message }) => {
                assert_eq!(got_url, url);
                assert_eq!(message, "moved");
            }
            other => panic!("expected Referral, got {other:?}"),
        }
    }

    #[test]
    fn bye_tag_maps_to_disconnected() {
        let resp = Response {
            tag: OkNoBye::Bye,
            code: None,
            human: Some("connection timeout".into()),
        };
        assert_eq!(
            classify_response(&resp),
            Err(Error::Disconnected("connection timeout".into()))
        );
    }

    #[test]
    fn no_without_code_is_other() {
        let resp = Response {
            tag: OkNoBye::No,
            code: None,
            human: Some("rejected".into()),
        };
        assert_eq!(
            classify_response(&resp),
            Err(Error::Other("rejected".into()))
        );
    }

    #[test]
    fn warnings_code_surfaces_as_warnings_variant() {
        // Note: in the wire protocol Warnings typically rides on an OK
        // response, but the No-with-Warnings shape is also legal per the
        // grammar. The classifier surfaces it either way; here we test the
        // No path since OK takes the success branch unconditionally.
        let resp = no_with(ResponseCode::Warnings, "compiled with warnings");
        assert_eq!(
            classify_response(&resp),
            Err(Error::Warnings("compiled with warnings".into()))
        );
    }
}
