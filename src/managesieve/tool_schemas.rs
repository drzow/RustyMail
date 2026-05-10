// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! JSON tool definitions for the seven sieve MCP tools.
//!
//! Returned by [`sieve_tool_definitions`] in the same shape as
//! `dashboard::api::handlers::get_mcp_tools_jsonrpc_format` — a
//! `Vec<Value>` where each element has `name`, `description`,
//! `inputSchema`. The MCP adapter merges these into the global
//! `list_tools` response.

use serde_json::{json, Value};

/// Tool-name prefix shared by every sieve tool.
pub const PREFIX: &str = "sieve_";

/// All seven sieve tools as JSON-RPC tool definitions.
pub fn sieve_tool_definitions() -> Vec<Value> {
    vec![
        capabilities_def(),
        list_scripts_def(),
        get_script_def(),
        put_script_def(),
        check_script_def(),
        set_active_def(),
        delete_script_def(),
    ]
}

fn capabilities_def() -> Value {
    json!({
        "name": "sieve_capabilities",
        "description": "Refresh and return the ManageSieve server's capability listing — \
            implementation/version strings, supported SIEVE extensions, advertised SASL \
            mechanisms, STARTTLS support, etc. Useful for verifying connectivity and \
            extension availability before generating a script.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

fn list_scripts_def() -> Value {
    json!({
        "name": "sieve_list_scripts",
        "description": "List every Sieve script stored on the server, with a flag indicating \
            which one is currently active. Returns {\"scripts\": [{\"name\": ..., \"active\": bool}, ...]}.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

fn get_script_def() -> Value {
    json!({
        "name": "sieve_get_script",
        "description": "Fetch a Sieve script's body by name. Returns {\"name\": ..., \"body\": ...}. \
            Errors with 'script not found' if the named script doesn't exist.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Script name to fetch (case-sensitive, server-stored name)."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }
    })
}

fn put_script_def() -> Value {
    json!({
        "name": "sieve_put_script",
        "description": "Upload a Sieve script. Replaces any existing script with the same name. \
            The server validates the body before storing; syntax errors come back as a server \
            rejection. Does NOT activate the script — call sieve_set_active for that.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Script name. Must be valid per RFC 5804 §1.6 \
                        (no control characters, no U+2028/U+2029)."
                },
                "body": {
                    "type": "string",
                    "description": "Sieve script source code (RFC 5228 grammar)."
                }
            },
            "required": ["name", "body"],
            "additionalProperties": false
        }
    })
}

fn check_script_def() -> Value {
    json!({
        "name": "sieve_check_script",
        "description": "Validate a Sieve script body without storing it. Returns \
            {\"status\": \"ok\"} on success; surfaces server-side syntax errors when the \
            script is invalid. Useful for pre-flighting a script before sieve_put_script.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "body": {
                    "type": "string",
                    "description": "Sieve script source to validate."
                }
            },
            "required": ["body"],
            "additionalProperties": false
        }
    })
}

fn set_active_def() -> Value {
    json!({
        "name": "sieve_set_active",
        "description": "Make the named script the active script (the one the server runs on \
            incoming mail). Pass an empty string to deactivate the current active script \
            without picking a replacement. Only one script can be active at a time.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Script to activate, or \"\" to deactivate any current active script."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }
    })
}

fn delete_script_def() -> Value {
    json!({
        "name": "sieve_delete_script",
        "description": "Delete a Sieve script. The server refuses to delete the currently-active \
            script — call sieve_set_active with a different name (or empty string) first.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Script name to delete."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defines_seven_tools() {
        assert_eq!(sieve_tool_definitions().len(), 7);
    }

    #[test]
    fn every_tool_name_starts_with_prefix() {
        for def in sieve_tool_definitions() {
            let name = def["name"].as_str().expect("name field");
            assert!(
                name.starts_with(PREFIX),
                "{name} does not start with {PREFIX}",
            );
        }
    }

    #[test]
    fn every_tool_defines_required_jsonrpc_fields() {
        for def in sieve_tool_definitions() {
            assert!(def.get("name").and_then(|v| v.as_str()).is_some());
            assert!(def.get("description").and_then(|v| v.as_str()).is_some());
            assert!(def
                .get("inputSchema")
                .and_then(|v| v.as_object())
                .is_some());
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let names: Vec<_> = sieve_tool_definitions()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate tool names");
    }
}
