// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! ManageSieve (RFC 5804) client and types.
//!
//! Wraps the `managesieve` crate (parser-only) with async TCP+TLS+SASL
//! transport, command/response loop, and converts wire types to rustymail
//! domain types. MCP tool surface lives in `crate::mcp::adapters::sieve`.

pub mod error;
pub mod parser;
pub mod types;

pub use error::Error;
pub use types::{Capabilities, SieveScript};
