// ABOUTME: Type definitions for Codex app-server protocol
// ABOUTME: Minimal JSON-RPC + request/response types for `codex app-server`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ============================================================================
// JSON-RPC Types
// ============================================================================

fn jsonrpc_v2() -> String {
    "2.0".to_string()
}

/// Request ID type (can be string, number, or null)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Null,
    String(String),
    Number(i64),
}

/// JSON-RPC message (any type)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Error(JsonRpcErrorResponse),
    Notification(JsonRpcNotification),
}

/// JSON-RPC request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "jsonrpc_v2")]
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC successful response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default = "jsonrpc_v2")]
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: serde_json::Value,
}

/// JSON-RPC error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    #[serde(default = "jsonrpc_v2")]
    pub jsonrpc: String,
    pub id: RequestId,
    pub error: JsonRpcError,
}

/// JSON-RPC error object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC notification (no id, no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    #[serde(default = "jsonrpc_v2")]
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ============================================================================
// Codex Protocol Types
// ============================================================================

/// Approval policy for command execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    #[serde(rename = "untrusted")]
    UnlessTrusted,
    OnFailure,
    OnRequest,
    Never,
}

/// Sandbox mode for command execution (kebab-case wire format)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// User input content
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text { text: String },
    Image { url: String },
    LocalImage { path: PathBuf },
}

/// Parameters for thread/start request
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<HashMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
}

/// Parameters for turn/start request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Parameters for turn/interrupt request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

/// Initialize params for the app-server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
}

/// Client info sent during initialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// MCP server configuration for passing to Codex app-server
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Server name (used as key in mcp_servers.<name>)
    pub name: String,
    /// Path to the executable
    pub command: String,
    /// Command arguments
    pub args: Vec<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    /// Convert to -c arguments for codex app-server
    pub fn to_config_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let prefix = format!("mcp_servers.{}", self.name);

        // Command
        args.push("-c".to_string());
        args.push(format!("{}.command=\"{}\"", prefix, self.command));

        // Args (as TOML array)
        if !self.args.is_empty() {
            let args_toml: Vec<String> = self.args.iter().map(|a| format!("\"{}\"", a)).collect();
            args.push("-c".to_string());
            args.push(format!("{}.args=[{}]", prefix, args_toml.join(", ")));
        }

        // Environment variables
        for (key, value) in &self.env {
            args.push("-c".to_string());
            args.push(format!("{}.env.{}=\"{}\"", prefix, key, value));
        }

        args
    }
}
