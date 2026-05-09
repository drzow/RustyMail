// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Domain types for ManageSieve operations.
//!
//! These are the shapes the rustymail client and MCP adapter expose to
//! callers. They are intentionally simpler than the underlying
//! `managesieve` crate's wire-level types.

use serde::{Deserialize, Serialize};

/// A Sieve script as known to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SieveScript {
    pub name: String,
    pub body: String,
    pub active: bool,
}

/// Server capabilities reported in the initial greeting and after STARTTLS.
///
/// This is a normalized view of `Vec<managesieve::Capability>`, hoisting
/// the well-known fields and bucketing the rest into `unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub implementation: Option<String>,
    pub version: Option<String>,
    pub sieve_extensions: Vec<String>,
    pub sasl_mechanisms: Vec<String>,
    pub notify_methods: Vec<String>,
    pub max_redirects: Option<usize>,
    pub starttls: bool,
    pub owner: Option<String>,
    pub language: Option<String>,
    /// Capabilities the server announced that the wrapper does not promote.
    /// Tuple = (name, optional argument string).
    pub unknown: Vec<(String, Option<String>)>,
}
