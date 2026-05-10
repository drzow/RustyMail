// Copyright (c) 2025 TexasFortress.AI
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

// src/mcp/adapters/sdk.rs

use async_trait::async_trait;
use std::sync::Arc;
use crate::prelude::CloneableImapSessionFactory;
use std::collections::HashMap;
use tokio::sync::Mutex as TokioMutex;
use serde_json::{Value, json};
use log::{debug, error, info, warn};

// Import RMCP SDK types
use rmcp::{
    model::*,
    service::RequestContext,
    ServerHandler,
    RoleServer,
};
use std::convert::TryInto;

// Use our MCP types
use crate::mcp::{McpPortState, JsonRpcRequest, JsonRpcResponse, JsonRpcError, McpHandler};
use crate::mcp_port::{create_mcp_tool_registry};

// Import session types
use tokio::sync::mpsc::UnboundedSender;
use crate::imap::error::ImapError;



// --- RustyMail Service Implementation ---
#[derive(Clone)]
pub struct RustyMailService {
    // State specific to this service
    pub port_state: Arc<TokioMutex<McpPortState>>,
    // Factory to create IMAP sessions on demand for tools
    pub session_factory: CloneableImapSessionFactory,
    // Tool registry containing all our MCP tools
    pub tool_registry: crate::mcp_port::McpToolRegistry,
}




impl RustyMailService {
    pub fn new(session_factory: CloneableImapSessionFactory) -> Self {
        let tool_registry = create_mcp_tool_registry();
        info!("RustyMailService: Tool registry created");

        Self {
            port_state: Arc::new(TokioMutex::new(McpPortState::default())),
            session_factory,
            tool_registry,
        }
    }

    // Wrapper method to call legacy MCP tools through the new SDK
    async fn execute_legacy_tool(&self, tool_name: String, params: Option<Value>) -> Result<CallToolResult, ErrorData> {
        debug!("Executing legacy tool '{}' via SDK", tool_name);

        let tool = self.tool_registry.get(&tool_name)
            .ok_or_else(|| ErrorData::new(
                ErrorCode(-32601), // Method not found
                format!("Tool '{}' not found", tool_name),
                None
            ))?;

        // Create IMAP session
        let session_result = self.session_factory.create_session().await;
        let client = match session_result {
            Ok(c) => c,
            Err(imap_err) => {
                error!("Failed to create IMAP session for tool '{}': {:?}", tool_name, imap_err);
                return Err(ErrorData::new(
                    ErrorCode(-32603), // Internal error
                    format!("IMAP connection failed: {}", imap_err),
                    None
                ));
            }
        };
        let session = client.session_arc();

        // Execute the tool
        let mut state_guard = self.port_state.lock().await;
        let result = tool.execute(session, &mut state_guard, params.unwrap_or(Value::Null)).await;
        drop(state_guard);

        // IMPORTANT: Logout to release BytePool buffers and prevent memory leak
        if let Err(e) = client.logout().await {
            warn!("Failed to logout IMAP session after tool execution: {}", e);
        }

        match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string());
                let content = Content {
                    raw: RawContent::Text(RawTextContent { text, meta: None }),
                    annotations: None,
                };
                Ok(CallToolResult::success(vec![content]))
            },
            Err(err) => Err(ErrorData::new(
                ErrorCode(err.code as i32),
                err.message,
                err.data
            ))
        }
    }
}

// Implement ServerHandler for the service
impl ServerHandler for RustyMailService {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: None,
                }),
                ..Default::default()
            },
            server_info: Implementation {
                name: "RustyMail MCP Server".to_string(),
                title: Some("RustyMail MCP".to_string()),
                version: "0.1.0".to_string(),
                description: Some("IMAP email client with MCP interface".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some("IMAP client with MCP interface for email operations".to_string()),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.to_string();
        // Sieve tools have their own connection lifecycle (open
        // ManageSieve session, authenticate, run op, log out) and
        // bypass the IMAP-coupled legacy tool registry.
        if name.starts_with(crate::managesieve::tool_schemas::PREFIX) {
            let args_value = request
                .arguments
                .map(|m| Value::Object(m.into_iter().collect()))
                .unwrap_or(Value::Object(serde_json::Map::new()));
            return self.dispatch_sieve_tool(name, args_value).await;
        }
        self.execute_legacy_tool(
            name,
            request
                .arguments
                .and_then(|m| m.into_iter().next().map(|(_, v)| v)),
        )
        .await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Pull tool definitions from the actual source of truth (handlers.rs + high_level_tools.rs)
        let low_level = crate::dashboard::api::handlers::get_mcp_tools_jsonrpc_format();
        let high_level = crate::dashboard::api::high_level_tools::get_mcp_high_level_tools_jsonrpc_format();
        let sieve = crate::managesieve::tool_schemas::sieve_tool_definitions();

        let mut seen_names = std::collections::HashSet::new();
        let mut items: Vec<Tool> = Vec::new();

        // Convert JSON tool definitions to rmcp Tool structs, deduplicating by name
        for tool_json in low_level.iter().chain(high_level.iter()).chain(sieve.iter()) {
            let name = tool_json.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
            if !seen_names.insert(name.to_string()) {
                continue; // Skip duplicates (high-level tools that also exist in low-level)
            }
            let description = tool_json.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let input_schema = tool_json.get("inputSchema")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            items.push(Tool {
                name: name.to_string().into(),
                title: Some(name.to_string()),
                description: Some(description.to_string().into()),
                input_schema: Arc::new(input_schema),
                output_schema: None,
                execution: None,
                icons: None,
                annotations: None,
                meta: None,
            });
        }

        Ok(ListToolsResult {
            tools: items,
            next_cursor: None,
            meta: None,
        })
    }
}

impl RustyMailService {
    /// Dispatch a sieve_* tool: resolve credentials, open a TLS-upgraded
    /// ManageSieve session, run the operation, log out, return the
    /// JSON result wrapped as MCP content. Each call opens and closes
    /// its own connection (no pooling in v1).
    async fn dispatch_sieve_tool(
        &self,
        tool_name: String,
        args: Value,
    ) -> Result<CallToolResult, ErrorData> {
        debug!("Executing sieve tool '{}' via SDK", tool_name);

        let creds = match resolve_sieve_credentials() {
            Ok(c) => c,
            Err(e) => {
                error!("sieve credentials lookup failed: {e}");
                return Err(ErrorData::new(
                    ErrorCode(-32603),
                    format!("sieve credentials: {e}"),
                    None,
                ));
            }
        };

        match crate::managesieve::mcp_tools::execute_sieve_tool(&creds, &tool_name, &args).await {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| "null".to_string());
                let content = Content {
                    raw: RawContent::Text(RawTextContent { text, meta: None }),
                    annotations: None,
                };
                Ok(CallToolResult::success(vec![content]))
            }
            Err(e) => {
                let code = sieve_error_code(&e);
                Err(ErrorData::new(ErrorCode(code), e.to_string(), None))
            }
        }
    }
}

/// Resolve sieve credentials at request time (so env-var changes are
/// picked up without restarting the MCP server). Reads
/// `RUSTYMAIL_ACCOUNTS_PATH` (default `config/accounts.json`) and
/// `RUSTYMAIL_SIEVE_ACCOUNT` (default = file's `default_account_id`).
fn resolve_sieve_credentials() -> Result<crate::managesieve::SieveCredentials, crate::managesieve::Error> {
    let path = std::env::var("RUSTYMAIL_ACCOUNTS_PATH")
        .unwrap_or_else(|_| "config/accounts.json".to_string());
    let account_id = std::env::var("RUSTYMAIL_SIEVE_ACCOUNT").ok();
    crate::managesieve::resolve_for_account(&path, account_id.as_deref())
}

/// Map our typed sieve errors to JSON-RPC error codes. Semantic
/// failures (script not found, etc.) get -32602 (invalid params);
/// auth failures get -32001 (custom); everything else falls through
/// to -32603 (internal error).
fn sieve_error_code(e: &crate::managesieve::Error) -> i32 {
    use crate::managesieve::Error::*;
    match e {
        ScriptNotFound(_) | ScriptAlreadyExists(_) | ScriptActive(_) => -32602,
        Auth(_) => -32001,
        _ => -32603,
    }
}

/// Adapter implementing McpHandler using the official RMCP SDK
pub struct SdkMcpAdapter {
    service: Arc<RustyMailService>,
}

impl SdkMcpAdapter {
    /// Creates a new SdkMcpAdapter.
    /// NOTE: Requires `CloneableImapSessionFactory` to be provided.
    pub fn new(session_factory: CloneableImapSessionFactory) -> Result<Self, Box<dyn std::error::Error>> {
        info!("Initializing SdkMcpAdapter...");
        let service = Arc::new(RustyMailService::new(session_factory));
        Ok(Self { service })
    }



}

#[async_trait]
impl McpHandler for SdkMcpAdapter {
    /// Handles an MCP request by delegating to the appropriate tool
    async fn handle_request(&self, state: Arc<TokioMutex<McpPortState>>, request: Value) -> Value {
        // Ensure input is a valid JsonRpcRequest structure before processing
        let rpc_request: JsonRpcRequest = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => {
                error!("SDK Adapter: Received invalid JSON-RPC request object: {}", e);
                return serde_json::to_value(JsonRpcResponse::invalid_request()).unwrap_or(json!(null));
            }
        };

        info!("SDK Adapter: Handling MCP request method: {}", rpc_request.method);

        // Update the service's state with the provided state
        *self.service.port_state.lock().await = state.lock().await.clone();

        // Handle the request using our legacy tool wrapper
        let params = rpc_request.params.clone();

        // Create a dummy context for the call
        // This is a workaround since we can't create RequestContext directly
        match self.service.execute_legacy_tool(
            rpc_request.method.clone(),
            params
        ).await {
            Ok(result) => {
                // Convert CallToolResult back to JsonRpcResponse
                let result_value = if !result.content.is_empty() {
                    json!({
                        "content": result.content.iter().map(|c| match c {
                            Content { raw: RawContent::Text(RawTextContent { ref text, .. }), .. } => json!({ "type": "text", "text": text }),
                            _ => json!(null),
                        }).collect::<Vec<_>>(),
                        "isError": result.is_error,
                    })
                } else {
                    json!(null)
                };

                let response = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: rpc_request.id,
                    result: Some(result_value),
                    error: None,
                };
                serde_json::to_value(response).unwrap_or(json!(null))
            }
            Err(err) => {
                let error_response = JsonRpcResponse::error(
                    rpc_request.id,
                    JsonRpcError {
                        code: -32603, // Internal error
                        message: err.message.into_owned(),
                        data: err.data,
                    }
                );
                serde_json::to_value(error_response).unwrap_or(json!(null))
            }
        }
    }
}

/// State specifically for the SdkMcpAdapter if needed (e.g., for SSE integration).
pub struct McpSdkState {
    pub session_factory: CloneableImapSessionFactory,
    pub sse_tx: Option<UnboundedSender<String>>,
    pub mcp_state: Arc<TokioMutex<McpPortState>>,
}
