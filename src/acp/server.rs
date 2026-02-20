// ABOUTME: ACP server implementation using official rust-sdk trait API
// ABOUTME: Implements acp::Agent trait to bridge ACP to Codex app-server

use crate::codex::{
    ApprovalPolicy, CodexClient, McpServerConfig, SandboxMode, ThreadStartParams, UserInput,
};
use agent_client_protocol::{self as acp, Client as _};
use base64::Engine as _;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug)]
enum AcpOutbound {
    SessionNotification(acp::SessionNotification),
    RequestPermission {
        request: acp::RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<acp::RequestPermissionResponse, acp::Error>>,
    },
}

/// Convert ACP MCP server configurations to Codex config format
fn convert_acp_mcp_to_codex(acp_servers: &[acp::McpServer]) -> Option<Vec<McpServerConfig>> {
    if acp_servers.is_empty() {
        return None;
    }

    let mut configs = Vec::new();
    for server in acp_servers {
        match server {
            acp::McpServer::Stdio(stdio) => {
                log::info!(
                    "Converting MCP server '{}': command={:?}, args={:?}",
                    stdio.name,
                    stdio.command,
                    stdio.args
                );
                let mut env = std::collections::HashMap::new();
                for var in &stdio.env {
                    env.insert(var.name.clone(), var.value.clone());
                }
                configs.push(McpServerConfig {
                    name: stdio.name.clone(),
                    command: stdio.command.to_string_lossy().to_string(),
                    args: stdio.args.clone(),
                    env,
                });
            }
            // HTTP and SSE are not currently supported for codex
            acp::McpServer::Http(_) | acp::McpServer::Sse(_) | _ => {
                log::warn!("HTTP/SSE MCP servers not supported for Codex, skipping");
            }
        }
    }

    if configs.is_empty() {
        None
    } else {
        Some(configs)
    }
}

/// Session data mapping ACP session to Codex thread
struct SessionData {
    thread_id: String,
    current_turn_id: Option<String>,
    #[allow(dead_code)]
    cwd: String,
    approval_policy: ApprovalPolicy,
    model: Option<String>,
    reasoning_effort: Option<String>,
    last_turn_diff: Option<String>,
    known_tool_calls: HashSet<String>,
}

/// Codex ACP Agent - bridges ACP protocol to Codex app-server
pub struct CodexAgent {
    /// Channel for sending outbound messages (notifications / permission requests) to the ACP client
    acp_tx: mpsc::UnboundedSender<AcpOutbound>,
    /// Codex client (lazily initialized)
    codex: RwLock<Option<Arc<CodexClient>>>,
    /// Session mappings: session_id -> SessionData
    sessions: RwLock<HashMap<String, SessionData>>,
}

impl CodexAgent {
    /// Create a new Codex agent
    fn new(acp_tx: mpsc::UnboundedSender<AcpOutbound>) -> Self {
        Self {
            acp_tx,
            codex: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Get or create Codex client
    async fn get_codex(
        &self,
        mcp_servers: Option<Vec<McpServerConfig>>,
    ) -> Result<Arc<CodexClient>, acp::Error> {
        // Check if already connected
        {
            let guard = self.codex.read().await;
            if let Some(ref client) = *guard {
                return Ok(Arc::clone(client));
            }
        }

        // Create new connection with MCP servers (only used on first connect)
        let client = CodexClient::connect("stdio", mcp_servers)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to connect to Codex: {}", e)))?;
        let client = Arc::new(client);
        {
            let mut guard = self.codex.write().await;
            *guard = Some(Arc::clone(&client));
        }
        Ok(client)
    }

    fn temp_dir_for_session(session_id: &acp::SessionId) -> PathBuf {
        // Keep it simple and avoid trusting session_id for path components.
        let mut dir = std::env::temp_dir();
        dir.push("seren-acp-codex");
        dir.push("sessions");
        dir.push(uuid::Uuid::new_v4().to_string());
        // Include the session id as a filename prefix in generated files only (not a directory).
        let _ = session_id;
        dir
    }

    fn image_extension_for_mime(mime_type: &str) -> &'static str {
        match mime_type {
            "image/png" => "png",
            "image/jpeg" | "image/jpg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "bin",
        }
    }

    async fn write_temp_image(
        &self,
        session_id: &acp::SessionId,
        mime_type: &str,
        data: &str,
    ) -> Result<PathBuf, acp::Error> {
        // ACP ImageContent.data is base64-encoded image bytes.
        let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .map_err(|e| {
                acp::Error::invalid_params().data(serde_json::Value::String(format!(
                    "Invalid base64 image data: {e}"
                )))
            })?;

        let mut dir = Self::temp_dir_for_session(session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| acp::Error::into_internal_error(e))?;

        let ext = Self::image_extension_for_mime(mime_type);
        let filename = format!("image-{}.{}", uuid::Uuid::new_v4(), ext);
        dir.push(filename);

        tokio::fs::write(&dir, bytes)
            .await
            .map_err(|e| acp::Error::into_internal_error(e))?;

        Ok(dir)
    }

    fn format_resource_link(link: &acp::ResourceLink) -> String {
        let mut out = String::new();
        out.push_str("[ResourceLink]\n");
        out.push_str(&format!("name: {}\n", link.name));
        out.push_str(&format!("uri: {}\n", link.uri));
        if let Some(title) = &link.title {
            out.push_str(&format!("title: {title}\n"));
        }
        if let Some(description) = &link.description {
            out.push_str(&format!("description: {description}\n"));
        }
        if let Some(mime_type) = &link.mime_type {
            out.push_str(&format!("mime: {mime_type}\n"));
        }
        if let Some(size) = link.size {
            out.push_str(&format!("size: {size}\n"));
        }
        out
    }

    fn format_embedded_resource(resource: &acp::EmbeddedResource) -> String {
        match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => {
                let mime = text.mime_type.as_deref().unwrap_or("text/plain");
                format!(
                    "[Resource]\nuri: {}\nmime: {}\n\n{}\n",
                    text.uri, mime, text.text
                )
            }
            acp::EmbeddedResourceResource::BlobResourceContents(blob) => {
                let mime = blob
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                format!(
                    "[Resource]\nuri: {}\nmime: {}\n\n(binary content: {} base64 chars)\n",
                    blob.uri,
                    mime,
                    blob.blob.len()
                )
            }
            _ => "[Resource]\n(unrecognized embedded resource)\n".to_string(),
        }
    }

    async fn content_blocks_to_user_input(
        &self,
        session_id: &acp::SessionId,
        blocks: &[acp::ContentBlock],
    ) -> Result<Vec<UserInput>, acp::Error> {
        let mut out = Vec::new();

        for block in blocks {
            match block {
                acp::ContentBlock::Text(text) => out.push(UserInput::Text {
                    text: text.text.clone(),
                }),
                acp::ContentBlock::ResourceLink(link) => out.push(UserInput::Text {
                    text: Self::format_resource_link(link),
                }),
                acp::ContentBlock::Resource(resource) => out.push(UserInput::Text {
                    text: Self::format_embedded_resource(resource),
                }),
                acp::ContentBlock::Image(image) => {
                    let path = self
                        .write_temp_image(session_id, &image.mime_type, &image.data)
                        .await?;
                    out.push(UserInput::LocalImage { path });
                }
                acp::ContentBlock::Audio(_) => {
                    return Err(acp::Error::invalid_params().data(serde_json::Value::String(
                        "Audio content blocks are not supported".to_string(),
                    )));
                }
                _ => {
                    // Ignore unknown / future content blocks.
                }
            }
        }

        if out.is_empty() {
            return Err(acp::Error::invalid_params().data(serde_json::Value::String(
                "Prompt contained no supported content blocks".to_string(),
            )));
        }

        Ok(out)
    }

    fn approval_policy_for_mode(mode_id: &str) -> ApprovalPolicy {
        match mode_id {
            // "Ask before running tools"
            "ask" => ApprovalPolicy::UnlessTrusted,
            // "Auto-approve safe operations"
            "auto" => ApprovalPolicy::OnFailure,
            _ => ApprovalPolicy::UnlessTrusted,
        }
    }

    fn sandbox_mode_from_env() -> SandboxMode {
        let Some(value) = std::env::var("SEREN_ACP_CODEX_SANDBOX").ok() else {
            return SandboxMode::WorkspaceWrite;
        };

        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "read-only" => SandboxMode::ReadOnly,
            "workspace-write" => SandboxMode::WorkspaceWrite,
            "danger-full-access" => SandboxMode::DangerFullAccess,
            _ => {
                log::warn!(
                    "Invalid SEREN_ACP_CODEX_SANDBOX value '{}'; defaulting to 'workspace-write'",
                    value
                );
                SandboxMode::WorkspaceWrite
            }
        }
    }

    fn session_modes() -> acp::SessionModeState {
        let ask_mode =
            acp::SessionMode::new("ask", "Ask").description("Ask before running tools".to_string());
        let auto_mode = acp::SessionMode::new("auto", "Auto")
            .description("Auto-approve safe operations".to_string());

        acp::SessionModeState::new("ask", vec![ask_mode, auto_mode])
    }

    fn parse_approval_policy(value: &str) -> Option<ApprovalPolicy> {
        match value {
            "untrusted" => Some(ApprovalPolicy::UnlessTrusted),
            "on-failure" => Some(ApprovalPolicy::OnFailure),
            "on-request" => Some(ApprovalPolicy::OnRequest),
            "never" => Some(ApprovalPolicy::Never),
            _ => None,
        }
    }

    async fn ensure_authenticated(&self, codex: &CodexClient) -> Result<(), acp::Error> {
        let account = codex
            .account_read(false)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to read account: {e}")))?;
        let requires_auth = account
            .get("requiresOpenaiAuth")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let has_account = account
            .get("account")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if requires_auth && !has_account {
            return Err(acp::Error::auth_required());
        }
        Ok(())
    }

    async fn build_model_state(
        &self,
        codex: &CodexClient,
        preferred_model: Option<&str>,
    ) -> Result<acp::SessionModelState, acp::Error> {
        let raw = codex
            .model_list(None, Some(200))
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to list models: {e}")))?;

        let items = raw
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| acp::Error::new(-32603, "Codex model/list response missing data"))?;

        let mut available = Vec::new();
        let mut default_model_id: Option<String> = None;

        for m in items {
            let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = m
                .get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            let description = m
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if m.get("isDefault")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                default_model_id = Some(id.to_string());
            }

            let mut info = acp::ModelInfo::new(acp::ModelId::new(id.to_string()), name);
            if let Some(desc) = description {
                info = info.description(desc);
            }
            available.push(info);
        }

        let current = preferred_model
            .map(|s| s.to_string())
            .filter(|id| available.iter().any(|m| m.model_id.0.as_ref() == id))
            .or(default_model_id)
            .or_else(|| available.first().map(|m| m.model_id.0.as_ref().to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        Ok(acp::SessionModelState::new(
            acp::ModelId::new(current),
            available,
        ))
    }

    async fn build_config_options(
        &self,
        codex: &CodexClient,
        current_model: Option<&str>,
        current_effort: Option<&str>,
    ) -> Result<Vec<acp::SessionConfigOption>, acp::Error> {
        let raw = codex
            .model_list(None, Some(200))
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to list models: {e}")))?;
        let items = raw.get("data").and_then(|v| v.as_array());

        let mut supported_efforts: Vec<acp::SessionConfigSelectOption> = Vec::new();
        let mut supported_effort_ids: Vec<String> = Vec::new();
        let mut default_effort: Option<String> = None;

        let mut model_match = None;
        if let Some(items) = items {
            for m in items {
                let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                if Some(id) == current_model {
                    model_match = Some(m);
                    break;
                }
                if model_match.is_none()
                    && m.get("isDefault")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                {
                    model_match = Some(m);
                }
            }
        }

        if let Some(m) = model_match {
            default_effort = m
                .get("defaultReasoningEffort")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if let Some(opts) = m
                .get("supportedReasoningEfforts")
                .and_then(|v| v.as_array())
            {
                for opt in opts {
                    let Some(effort) = opt.get("reasoningEffort").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let desc = opt
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let name = effort.to_string();
                    let mut entry = acp::SessionConfigSelectOption::new(effort.to_string(), name);
                    if let Some(d) = desc {
                        entry = entry.description(d);
                    }
                    supported_efforts.push(entry);
                    supported_effort_ids.push(effort.to_string());
                }
            }
        }

        if supported_efforts.is_empty() {
            for effort in ["none", "minimal", "low", "medium", "high", "xhigh"] {
                supported_efforts.push(acp::SessionConfigSelectOption::new(
                    effort,
                    effort.to_string(),
                ));
                supported_effort_ids.push(effort.to_string());
            }
        }

        let desired_effort = current_effort
            .map(|s| s.to_string())
            .or_else(|| default_effort.clone())
            .unwrap_or_else(|| "medium".to_string());
        let chosen_effort = Self::choose_supported_effort(
            desired_effort,
            default_effort.as_deref(),
            &supported_effort_ids,
        );

        let reasoning = acp::SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning effort",
            chosen_effort,
            supported_efforts,
        )
        .category(acp::SessionConfigOptionCategory::ThoughtLevel)
        .description("Codex reasoning effort override for this session.");

        Ok(vec![reasoning])
    }

    fn reasoning_effort_rank(effort: &str) -> Option<u8> {
        match effort {
            "none" => Some(0),
            "minimal" => Some(1),
            "low" => Some(2),
            "medium" => Some(3),
            "high" => Some(4),
            "xhigh" => Some(5),
            _ => None,
        }
    }

    fn choose_supported_effort(
        desired: String,
        default_effort: Option<&str>,
        supported: &[String],
    ) -> String {
        if supported.is_empty() {
            return desired;
        }
        if supported.iter().any(|v| v == &desired) {
            return desired;
        }

        // Prefer the closest supported effort that is <= desired (so "xhigh" becomes "high").
        if let Some(dr) = Self::reasoning_effort_rank(&desired) {
            let mut best: Option<(u8, &String)> = None;
            for v in supported {
                let Some(r) = Self::reasoning_effort_rank(v) else {
                    continue;
                };
                if r <= dr {
                    best = match best {
                        None => Some((r, v)),
                        Some((best_r, _)) if r > best_r => Some((r, v)),
                        Some(prev) => Some(prev),
                    };
                }
            }
            if let Some((_, v)) = best {
                return v.clone();
            }
        }

        // Fall back to model default if present, otherwise the first option.
        if let Some(def) = default_effort {
            if supported.iter().any(|v| v == def) {
                return def.to_string();
            }
        }
        supported[0].clone()
    }

    fn extract_reasoning_effort_from_config_options(
        config_options: &[acp::SessionConfigOption],
    ) -> Option<String> {
        for opt in config_options {
            if opt.id.0.as_ref() != "reasoning_effort" {
                continue;
            }
            let acp::SessionConfigKind::Select(sel) = &opt.kind else {
                continue;
            };
            return Some(sel.current_value.0.as_ref().to_string());
        }
        None
    }

    fn replay_chunk_meta(item: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert("replay".to_string(), serde_json::Value::Bool(true));

        if let Some(message_id) = item.get("id").and_then(|v| v.as_str()) {
            meta.insert(
                "messageId".to_string(),
                serde_json::Value::String(message_id.to_string()),
            );
        }

        if let Some(timestamp) = item
            .get("timestamp")
            .or_else(|| item.get("createdAt"))
            .or_else(|| item.get("created_at"))
            .cloned()
        {
            meta.insert("timestamp".to_string(), timestamp);
        }

        meta
    }

    async fn replay_thread_items(
        &self,
        session_id: &acp::SessionId,
        thread: &serde_json::Value,
    ) -> Result<(), acp::Error> {
        if let Some(turns) = thread.get("turns").and_then(|v| v.as_array()) {
            for turn in turns {
                if let Some(items) = turn.get("items").and_then(|v| v.as_array()) {
                    for item in items {
                        self.replay_item(session_id, item).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn replay_item(
        &self,
        session_id: &acp::SessionId,
        item: &serde_json::Value,
    ) -> Result<(), acp::Error> {
        let Some(item_type) = item.get("type").and_then(|v| v.as_str()) else {
            return Ok(());
        };

        let replay_meta = Self::replay_chunk_meta(item);

        match item_type {
            "userMessage" => {
                if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            self.emit_session_update(
                                session_id.clone(),
                                acp::SessionUpdate::UserMessageChunk(
                                    acp::ContentChunk::new(acp::ContentBlock::Text(
                                        acp::TextContent::new(text.to_string()),
                                    ))
                                    .meta(replay_meta.clone()),
                                ),
                            )
                            .await?;
                        }
                    }
                }
            }
            "agentMessage" => {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                self.emit_session_update(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(
                        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                            text.to_string(),
                        )))
                        .meta(replay_meta.clone()),
                    ),
                )
                .await?;
            }
            "reasoning" => {
                let parts = item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let text = parts
                    .into_iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    self.emit_session_update(
                        session_id.clone(),
                        acp::SessionUpdate::AgentThoughtChunk(
                            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                                text,
                            )))
                            .meta(replay_meta.clone()),
                        ),
                    )
                    .await?;
                }
            }
            // Tool calls: best-effort replay for rich UI.
            "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "webSearch"
            | "imageView"
            | "enteredReviewMode"
            | "exitedReviewMode"
            | "contextCompaction"
            | "collabAgentToolCall" => {
                self.replay_tool_item(session_id, item).await?;
            }
            _ => {}
        }

        Ok(())
    }

    async fn replay_tool_item(
        &self,
        session_id: &acp::SessionId,
        item: &serde_json::Value,
    ) -> Result<(), acp::Error> {
        let Some(item_id) = item.get("id").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let Some(item_type) = item.get("type").and_then(|v| v.as_str()) else {
            return Ok(());
        };

        let (kind, title, status) = Self::tool_call_fields_for_item(item_type, item);
        let tool_call_id_str = Self::tool_call_id_string_for_codex_item(item_id);
        let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

        // Avoid duplicating tool calls if we already emitted them.
        let first_seen = {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &session_id.0;
            sessions
                .get_mut(session_id_str)
                .map(|s| s.known_tool_calls.insert(tool_call_id_str.clone()))
                .unwrap_or(true)
        };

        if first_seen {
            self.emit_session_update(
                session_id.clone(),
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), title.clone())
                        .kind(kind)
                        .status(status)
                        .raw_input(item.clone()),
                ),
            )
            .await?;
        }

        let mut fields = acp::ToolCallUpdateFields::new()
            .kind(kind)
            .status(status)
            .raw_output(item.clone());

        if item_type == "fileChange" {
            if let Some(content) = self
                .diff_content_from_file_change_item(session_id, item)
                .await
            {
                fields = fields.content(content);
            }
        }

        self.emit_session_update(
            session_id.clone(),
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(tool_call_id, fields)),
        )
        .await?;

        Ok(())
    }

    fn tool_call_fields_for_item(
        item_type: &str,
        item: &serde_json::Value,
    ) -> (acp::ToolKind, String, acp::ToolCallStatus) {
        match item_type {
            "commandExecution" => {
                let command = item
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        item.get("command").and_then(|v| v.as_array()).map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
                    .unwrap_or_default();
                let title = if command.is_empty() {
                    "Run command".to_string()
                } else {
                    format!("Run command: {command}")
                };
                let status = item.get("status").and_then(|v| v.as_str());
                let status = match status {
                    Some("failed") | Some("declined") => acp::ToolCallStatus::Failed,
                    Some("completed") => acp::ToolCallStatus::Completed,
                    _ => acp::ToolCallStatus::InProgress,
                };
                (acp::ToolKind::Execute, title, status)
            }
            "fileChange" => {
                let title = "Apply file changes".to_string();
                let status = item.get("status").and_then(|v| v.as_str());
                let status = match status {
                    Some("failed") | Some("declined") => acp::ToolCallStatus::Failed,
                    Some("completed") => acp::ToolCallStatus::Completed,
                    _ => acp::ToolCallStatus::InProgress,
                };
                (acp::ToolKind::Edit, title, status)
            }
            "mcpToolCall" => {
                let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("mcp");
                let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
                let title = format!("MCP: {server}.{tool}");
                let status = item.get("status").and_then(|v| v.as_str());
                let status = match status {
                    Some("failed") => acp::ToolCallStatus::Failed,
                    Some("completed") => acp::ToolCallStatus::Completed,
                    _ => acp::ToolCallStatus::InProgress,
                };
                (acp::ToolKind::Fetch, title, status)
            }
            "webSearch" => {
                let query = item.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let title = if query.is_empty() {
                    "Web search".to_string()
                } else {
                    format!("Web search: {query}")
                };
                (acp::ToolKind::Search, title, acp::ToolCallStatus::Completed)
            }
            "imageView" => {
                let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let title = if path.is_empty() {
                    "View image".to_string()
                } else {
                    format!("View image: {path}")
                };
                (acp::ToolKind::Read, title, acp::ToolCallStatus::Completed)
            }
            "enteredReviewMode" => (
                acp::ToolKind::Think,
                "Entered review mode".to_string(),
                acp::ToolCallStatus::Completed,
            ),
            "exitedReviewMode" => (
                acp::ToolKind::Think,
                "Exited review mode".to_string(),
                acp::ToolCallStatus::Completed,
            ),
            "contextCompaction" => (
                acp::ToolKind::Think,
                "Context compaction".to_string(),
                acp::ToolCallStatus::Completed,
            ),
            "collabAgentToolCall" => (
                acp::ToolKind::Other,
                "Collaborative tool call".to_string(),
                acp::ToolCallStatus::Completed,
            ),
            _ => (
                acp::ToolKind::Other,
                "Tool call".to_string(),
                acp::ToolCallStatus::Completed,
            ),
        }
    }

    async fn diff_content_from_file_change_item(
        &self,
        session_id: &acp::SessionId,
        item: &serde_json::Value,
    ) -> Option<Vec<acp::ToolCallContent>> {
        let changes = item.get("changes").and_then(|v| v.as_array())?;

        let cwd = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id.0.as_ref()).map(|s| s.cwd.clone())
        };
        let cwd = cwd.unwrap_or_else(|| ".".to_string());

        let mut content = Vec::new();
        for change in changes {
            let Some(path) = change.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            let diff = change.get("diff").and_then(|v| v.as_str()).unwrap_or("");
            let (old_text, new_text) = Self::unified_diff_to_old_new(diff);
            let abs_path = std::path::PathBuf::from(&cwd).join(path);
            let mut d = acp::Diff::new(abs_path, new_text);
            if let Some(old) = old_text {
                d = d.old_text(old);
            }
            content.push(acp::ToolCallContent::Diff(d));
        }

        if content.is_empty() {
            None
        } else {
            Some(content)
        }
    }

    fn unified_diff_to_old_new(diff: &str) -> (Option<String>, String) {
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        let mut in_hunk = false;

        for line in diff.lines() {
            if line.starts_with("@@") {
                in_hunk = true;
                continue;
            }
            if !in_hunk {
                continue;
            }
            if line.starts_with("\\ No newline") {
                continue;
            }
            match line.chars().next() {
                Some(' ') => {
                    old_lines.push(line[1..].to_string());
                    new_lines.push(line[1..].to_string());
                }
                Some('-') => old_lines.push(line[1..].to_string()),
                Some('+') => new_lines.push(line[1..].to_string()),
                _ => {}
            }
        }

        let old = if old_lines.is_empty() {
            None
        } else {
            Some(old_lines.join("\n"))
        };
        (old, new_lines.join("\n"))
    }

    fn strip_git_diff_path(path: &str) -> Option<String> {
        if path == "/dev/null" {
            return None;
        }
        Some(
            path.strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(path)
                .to_string(),
        )
    }

    fn diff_section_path(section: &str) -> Option<String> {
        let mut old_path: Option<String> = None;
        let mut new_path: Option<String> = None;

        for line in section.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                new_path = Self::strip_git_diff_path(rest.trim());
            } else if let Some(rest) = line.strip_prefix("--- ") {
                old_path = Self::strip_git_diff_path(rest.trim());
            }
        }

        new_path.or(old_path)
    }

    fn split_unified_diff_sections(diff: &str) -> Vec<String> {
        let mut sections: Vec<Vec<String>> = Vec::new();
        let mut current: Vec<String> = Vec::new();

        for line in diff.lines() {
            if line.starts_with("diff --git ") && !current.is_empty() {
                sections.push(current);
                current = Vec::new();
            }
            current.push(line.to_string());
        }
        if !current.is_empty() {
            sections.push(current);
        }

        if sections.is_empty() {
            return vec![];
        }

        sections
            .into_iter()
            .map(|lines| lines.join("\n"))
            .collect::<Vec<_>>()
    }

    fn diff_content_from_unified_diff(cwd: &str, diff: &str) -> Vec<acp::ToolCallContent> {
        let mut content = Vec::new();

        for section in Self::split_unified_diff_sections(diff) {
            let Some(path) = Self::diff_section_path(&section) else {
                continue;
            };
            let (old_text, new_text) = Self::unified_diff_to_old_new(&section);
            let abs_path = std::path::PathBuf::from(cwd).join(path);
            let mut d = acp::Diff::new(abs_path, new_text);
            if let Some(old) = old_text {
                d = d.old_text(old);
            }
            content.push(acp::ToolCallContent::Diff(d));
        }

        content
    }

    fn tool_call_id_string_for_codex_item(item_id: &str) -> String {
        format!("codex-item:{item_id}")
    }

    async fn emit_session_update(
        &self,
        session_id: acp::SessionId,
        update: acp::SessionUpdate,
    ) -> Result<(), acp::Error> {
        self.acp_tx
            .send(AcpOutbound::SessionNotification(
                acp::SessionNotification::new(session_id, update),
            ))
            .map_err(|_| acp::Error::new(-32603, "ACP outbound channel closed"))?;
        Ok(())
    }

    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        let (response_tx, response_rx) = oneshot::channel();

        self.acp_tx
            .send(AcpOutbound::RequestPermission {
                request,
                response_tx,
            })
            .map_err(|_| acp::Error::new(-32603, "ACP outbound channel closed"))?;

        response_rx
            .await
            .map_err(|_| acp::Error::new(-32603, "ACP permission request cancelled"))?
    }

    async fn handle_codex_request_notification(
        &self,
        codex: &CodexClient,
        session_id: &acp::SessionId,
        notif: &crate::codex::JsonRpcNotification,
    ) -> Result<(), acp::Error> {
        let method = notif
            .method
            .strip_prefix("request:")
            .unwrap_or(&notif.method);
        let Some(wrapper) = notif.params.as_ref() else {
            return Ok(());
        };

        let request_id = wrapper
            .get("id")
            .and_then(|v| serde_json::from_value::<crate::codex::RequestId>(v.clone()).ok())
            .ok_or_else(|| acp::Error::new(-32603, "Invalid Codex request id"))?;
        let params = wrapper
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        match method {
            "item/commandExecution/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let command = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("");

                let title = if command.is_empty() {
                    "Run command".to_string()
                } else {
                    format!("Run command: {command}")
                };
                let subtitle = if reason.is_empty() {
                    title.clone()
                } else {
                    format!("{title}\nReason: {reason}")
                };

                let tool_call_id_str = Self::tool_call_id_string_for_codex_item(item_id);
                let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

                let should_create_tool_call = {
                    let mut sessions = self.sessions.write().await;
                    let session_id_str: &str = &session_id.0;
                    sessions
                        .get_mut(session_id_str)
                        .map(|s| s.known_tool_calls.insert(tool_call_id_str.clone()))
                        .unwrap_or(true)
                };

                // Ensure the client sees a tool call entry before permission is requested.
                if should_create_tool_call {
                    self.emit_session_update(
                        session_id.clone(),
                        acp::SessionUpdate::ToolCall(
                            acp::ToolCall::new(tool_call_id.clone(), title.clone())
                                .kind(acp::ToolKind::Execute)
                                .raw_input(params.clone()),
                        ),
                    )
                    .await?;
                }

                let tool_call = acp::ToolCallUpdate::new(
                    tool_call_id.clone(),
                    acp::ToolCallUpdateFields::new()
                        .title(subtitle.clone())
                        .kind(acp::ToolKind::Execute)
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(params.clone()),
                );

                let options = vec![
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-once"),
                        "Allow once",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-session"),
                        "Allow for session",
                        acp::PermissionOptionKind::AllowAlways,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("reject"),
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("cancel"),
                        "Reject and cancel turn",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ];

                let response = self
                    .request_permission(acp::RequestPermissionRequest::new(
                        session_id.clone(),
                        tool_call,
                        options,
                    ))
                    .await?;

                let decision = match response.outcome {
                    acp::RequestPermissionOutcome::Cancelled => "cancel",
                    acp::RequestPermissionOutcome::Selected(selected) => {
                        match selected.option_id.0.as_ref() {
                            "allow-once" => "accept",
                            "allow-session" => "acceptForSession",
                            "cancel" => "cancel",
                            _ => "decline",
                        }
                    }
                    _ => "cancel",
                };

                codex
                    .respond(request_id, serde_json::json!({ "decision": decision }))
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to respond: {e}")))?;

                // Update status so the UI doesn't leave it in "Pending" forever.
                let status = match decision {
                    "accept" | "acceptForSession" => acp::ToolCallStatus::InProgress,
                    "cancel" => acp::ToolCallStatus::Failed,
                    "decline" => acp::ToolCallStatus::Completed,
                    _ => acp::ToolCallStatus::Failed,
                };
                self.emit_session_update(
                    session_id.clone(),
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        tool_call_id,
                        acp::ToolCallUpdateFields::new().status(status),
                    )),
                )
                .await?;
            }
            "item/fileChange/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                let title = if reason.is_empty() {
                    "Apply file changes".to_string()
                } else {
                    format!("Apply file changes\nReason: {reason}")
                };
                let tool_call_id_str = Self::tool_call_id_string_for_codex_item(item_id);
                let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

                let should_create_tool_call = {
                    let mut sessions = self.sessions.write().await;
                    let session_id_str: &str = &session_id.0;
                    sessions
                        .get_mut(session_id_str)
                        .map(|s| s.known_tool_calls.insert(tool_call_id_str.clone()))
                        .unwrap_or(true)
                };

                if should_create_tool_call {
                    self.emit_session_update(
                        session_id.clone(),
                        acp::SessionUpdate::ToolCall(
                            acp::ToolCall::new(tool_call_id.clone(), "Apply file changes")
                                .kind(acp::ToolKind::Edit)
                                .raw_input(params.clone()),
                        ),
                    )
                    .await?;
                }

                let tool_call = acp::ToolCallUpdate::new(
                    tool_call_id.clone(),
                    acp::ToolCallUpdateFields::new()
                        .title(title)
                        .kind(acp::ToolKind::Edit)
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(params.clone()),
                );

                let options = vec![
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-once"),
                        "Allow once",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("allow-session"),
                        "Allow for session",
                        acp::PermissionOptionKind::AllowAlways,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("reject"),
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                    acp::PermissionOption::new(
                        acp::PermissionOptionId::new("cancel"),
                        "Reject and cancel turn",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ];

                let response = self
                    .request_permission(acp::RequestPermissionRequest::new(
                        session_id.clone(),
                        tool_call,
                        options,
                    ))
                    .await?;

                let decision = match response.outcome {
                    acp::RequestPermissionOutcome::Cancelled => "cancel",
                    acp::RequestPermissionOutcome::Selected(selected) => {
                        match selected.option_id.0.as_ref() {
                            "allow-once" => "accept",
                            "allow-session" => "acceptForSession",
                            "cancel" => "cancel",
                            _ => "decline",
                        }
                    }
                    _ => "cancel",
                };

                codex
                    .respond(request_id, serde_json::json!({ "decision": decision }))
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to respond: {e}")))?;

                let status = match decision {
                    "accept" | "acceptForSession" => acp::ToolCallStatus::InProgress,
                    "cancel" => acp::ToolCallStatus::Failed,
                    "decline" => acp::ToolCallStatus::Completed,
                    _ => acp::ToolCallStatus::Failed,
                };
                self.emit_session_update(
                    session_id.clone(),
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        tool_call_id,
                        acp::ToolCallUpdateFields::new().status(status),
                    )),
                )
                .await?;
            }
            _ => {
                codex
                    .respond_error(
                        request_id,
                        -32601,
                        format!("Unsupported Codex request method: {method}"),
                        None,
                    )
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to respond: {e}")))?;
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for CodexAgent {
    async fn initialize(
        &self,
        request: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        log::info!("Received initialize request");

        let protocol_version = if request.protocol_version < acp::ProtocolVersion::V1 {
            // Treat V0 (pre-release) as unsupported; respond with our supported version.
            acp::ProtocolVersion::V1
        } else {
            std::cmp::min(request.protocol_version, acp::ProtocolVersion::LATEST)
        };

        // Build capabilities
        let prompt_caps = acp::PromptCapabilities::new()
            .embedded_context(true)
            .image(true);
        let mut agent_caps = acp::AgentCapabilities::new()
            .prompt_capabilities(prompt_caps)
            .mcp_capabilities(acp::McpCapabilities::new())
            .load_session(true);
        agent_caps.session_capabilities =
            acp::SessionCapabilities::new().list(acp::SessionListCapabilities::new());

        let mut auth_methods = vec![
            acp::AuthMethod::new("chatgpt", "Login with ChatGPT")
                .description("Browser-based login for Codex CLI (ChatGPT account)."),
            acp::AuthMethod::new("codex-api-key", "Use CODEX_API_KEY")
                .description("Requires setting the CODEX_API_KEY environment variable."),
            acp::AuthMethod::new("openai-api-key", "Use OPENAI_API_KEY")
                .description("Requires setting the OPENAI_API_KEY environment variable."),
        ];
        // Headless environments may not be able to complete browser login.
        if std::env::var("NO_BROWSER").is_ok() {
            auth_methods.retain(|m| m.id.0.as_ref() != "chatgpt");
        }

        // Build agent info
        let agent_info = acp::Implementation::new("seren-acp-codex", env!("CARGO_PKG_VERSION"))
            .title("Codex Agent".to_string());

        Ok(acp::InitializeResponse::new(protocol_version)
            .agent_capabilities(agent_caps)
            .agent_info(agent_info)
            .auth_methods(auth_methods))
    }

    async fn authenticate(
        &self,
        request: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        log::info!("Received authenticate request");

        fn open_auth_url(url: &str) {
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(url).spawn();
            }
            #[cfg(target_os = "linux")]
            {
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "start", url])
                    .spawn();
            }
        }

        let codex = self.get_codex(None).await?;

        // If we're already authenticated with a compatible method, treat this as a no-op.
        if let Ok(account) = codex.account_read(false).await {
            let current_type = account
                .get("account")
                .and_then(|v| v.as_object())
                .and_then(|a| a.get("type"))
                .and_then(|t| t.as_str());
            match (current_type, request.method_id.0.as_ref()) {
                (Some("chatgpt"), "chatgpt") => return Ok(acp::AuthenticateResponse::new()),
                (Some("apiKey"), "codex-api-key" | "openai-api-key") => {
                    return Ok(acp::AuthenticateResponse::new());
                }
                _ => {}
            }
        }

        match request.method_id.0.as_ref() {
            "chatgpt" => {
                // Subscribe before initiating the login so we don't miss the completion event.
                let mut rx = codex.subscribe();
                let result = codex
                    .account_login_start(serde_json::json!({ "type": "chatgpt" }))
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to start login: {e}")))?;

                let auth_url = result
                    .get("authUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::new(-32603, "Codex login response missing authUrl")
                    })?;
                let login_id = result
                    .get("loginId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::new(-32603, "Codex login response missing loginId")
                    })?;

                log::info!("Starting browser login flow for Codex (loginId={login_id})");
                open_auth_url(auth_url);

                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(10 * 60);
                loop {
                    let recv = tokio::time::timeout_at(deadline, rx.recv()).await;
                    let notif = match recv {
                        Ok(Ok(n)) => n,
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                            return Err(acp::Error::new(-32603, "Codex process closed"));
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                        Err(_) => {
                            let _ = codex.account_login_cancel(login_id).await;
                            return Err(acp::Error::new(-32603, "Login timed out"));
                        }
                    };

                    if notif.method != "account/login/completed" {
                        continue;
                    }
                    let Some(params) = notif.params.as_ref() else {
                        continue;
                    };

                    // Ignore completion notifications for other login attempts.
                    let completed_login_id =
                        params.get("loginId").and_then(|v| v.as_str()).unwrap_or("");
                    if completed_login_id != login_id {
                        continue;
                    }

                    let success = params
                        .get("success")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if success {
                        return Ok(acp::AuthenticateResponse::new());
                    }
                    let error = params
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Login failed");
                    return Err(acp::Error::new(-32603, error));
                }
            }
            "codex-api-key" => {
                let api_key = std::env::var("CODEX_API_KEY").map_err(|_| {
                    acp::Error::internal_error()
                        .data(serde_json::Value::String("CODEX_API_KEY is not set".into()))
                })?;
                codex
                    .account_login_start(serde_json::json!({ "type": "apiKey", "apiKey": api_key }))
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to login: {e}")))?;
                Ok(acp::AuthenticateResponse::new())
            }
            "openai-api-key" => {
                let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                    acp::Error::internal_error().data(serde_json::Value::String(
                        "OPENAI_API_KEY is not set".into(),
                    ))
                })?;
                codex
                    .account_login_start(serde_json::json!({ "type": "apiKey", "apiKey": api_key }))
                    .await
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to login: {e}")))?;
                Ok(acp::AuthenticateResponse::new())
            }
            _ => Err(acp::Error::invalid_params().data(serde_json::Value::String(
                "Unsupported authentication method".into(),
            ))),
        }
    }

    async fn new_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        log::info!("Received new session request for cwd: {:?}", request.cwd);
        log::info!("MCP servers from client: {}", request.mcp_servers.len());

        // Convert ACP MCP servers to Codex config format
        let mcp_servers = convert_acp_mcp_to_codex(&request.mcp_servers);

        let codex = self.get_codex(mcp_servers).await?;

        // Ensure we are authenticated if Codex requires it.
        self.ensure_authenticated(&codex).await?;

        // Start a Codex thread
        let cwd = request.cwd.to_string_lossy().to_string();
        let default_approval_policy = Self::approval_policy_for_mode("ask");
        let sandbox = Self::sandbox_mode_from_env();
        log::info!("Using Codex sandbox mode: {:?}", sandbox);
        let thread_params = ThreadStartParams {
            cwd: Some(cwd.clone()),
            model: None,
            model_provider: None,
            approval_policy: Some(default_approval_policy),
            sandbox: Some(sandbox),
            config: None,
            base_instructions: None,
            developer_instructions: None,
        };

        let thread_start = codex
            .thread_start_full(thread_params)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to start Codex thread: {}", e)))?;

        let thread_id = thread_start
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| acp::Error::new(-32603, "No thread.id in response"))?
            .to_string();

        let approval_policy = thread_start
            .get("approvalPolicy")
            .and_then(|v| v.as_str())
            .and_then(Self::parse_approval_policy)
            .unwrap_or(default_approval_policy);

        let model = thread_start
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let reasoning_effort = thread_start
            .get("reasoningEffort")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        log::info!("Created Codex thread {} for cwd {}", thread_id, cwd);

        // Use the Codex thread id as the ACP session id so it can be persisted and reloaded.
        let session_id = thread_id.clone();

        let models = self.build_model_state(&codex, model.as_deref()).await?;
        let config_options = self
            .build_config_options(&codex, model.as_deref(), reasoning_effort.as_deref())
            .await?;

        // Clamp any stale/unsupported effort to a supported value for this model.
        let resolved_reasoning_effort =
            Self::extract_reasoning_effort_from_config_options(&config_options)
                .or(reasoning_effort.clone());

        // Store session mapping
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    thread_id: thread_id.clone(),
                    current_turn_id: None,
                    cwd,
                    approval_policy,
                    model: model.clone(),
                    reasoning_effort: resolved_reasoning_effort,
                    last_turn_diff: None,
                    known_tool_calls: HashSet::new(),
                },
            );
        }

        Ok(acp::NewSessionResponse::new(session_id)
            .modes(Self::session_modes())
            .models(models)
            .config_options(config_options))
    }

    async fn load_session(
        &self,
        request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        log::info!("Received load session request");

        // Convert ACP MCP servers to Codex config format (only used on first connect).
        let mcp_servers = convert_acp_mcp_to_codex(&request.mcp_servers);
        let codex = self.get_codex(mcp_servers).await?;

        self.ensure_authenticated(&codex).await?;

        let thread_id: String = request.session_id.0.as_ref().to_string();
        let cwd = request.cwd.to_string_lossy().to_string();

        let resume = codex
            .thread_resume(crate::codex::ThreadResumeParams {
                thread_id: thread_id.clone(),
                cwd: Some(cwd.clone()),
                ..Default::default()
            })
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to resume thread: {e}")))?;

        let approval_policy = resume
            .get("approvalPolicy")
            .and_then(|v| v.as_str())
            .and_then(Self::parse_approval_policy)
            .unwrap_or(Self::approval_policy_for_mode("ask"));
        let model = resume
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let reasoning_effort = resume
            .get("reasoningEffort")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Store session mapping (session id == thread id).
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                thread_id.clone(),
                SessionData {
                    thread_id: thread_id.clone(),
                    current_turn_id: None,
                    cwd: cwd.clone(),
                    approval_policy,
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    last_turn_diff: None,
                    known_tool_calls: HashSet::new(),
                },
            );
        }

        // Replay history to the client.
        if let Some(thread) = resume.get("thread") {
            self.replay_thread_items(&request.session_id, thread)
                .await?;
        }

        let models = self.build_model_state(&codex, model.as_deref()).await?;
        let config_options = self
            .build_config_options(&codex, model.as_deref(), reasoning_effort.as_deref())
            .await?;

        if let Some(resolved_effort) =
            Self::extract_reasoning_effort_from_config_options(&config_options)
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(thread_id.as_str()) {
                session.reasoning_effort = Some(resolved_effort);
            }
        }

        Ok(acp::LoadSessionResponse::new()
            .modes(Self::session_modes())
            .models(models)
            .config_options(config_options))
    }

    async fn prompt(&self, request: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        log::info!(
            "Received prompt request for session: {:?}",
            request.session_id
        );

        let codex = self.get_codex(None).await?;

        // Get thread ID and session config for this session
        let (thread_id, approval_policy, model, reasoning_effort) = {
            let sessions = self.sessions.read().await;
            let session_id_str: &str = &request.session_id.0;
            sessions
                .get(session_id_str)
                .map(|s| {
                    (
                        s.thread_id.clone(),
                        s.approval_policy,
                        s.model.clone(),
                        s.reasoning_effort.clone(),
                    )
                })
                .ok_or_else(|| {
                    acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
                })?
        };

        // Convert ACP content blocks to Codex user input.
        let user_input = self
            .content_blocks_to_user_input(&request.session_id, &request.prompt)
            .await?;

        log::debug!(
            "Sending prompt to Codex thread {}: {:?}",
            thread_id,
            user_input
        );

        // Subscribe to notifications before sending message, so we don't miss early deltas.
        let mut rx = codex.subscribe();

        // Start turn with user input
        log::info!(
            "Starting Codex turn: thread_id={}, model={}, reasoning_effort={}, approval_policy={:?}",
            thread_id,
            model.as_deref().unwrap_or("<default>"),
            reasoning_effort.as_deref().unwrap_or("<default>"),
            approval_policy
        );
        let turn_id = codex
            .turn_start_with_overrides(
                &thread_id,
                user_input,
                Some(approval_policy),
                model.clone(),
                reasoning_effort.clone(),
                None,
            )
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to start turn: {}", e)))?;

        // Store the turn ID for potential cancellation
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.current_turn_id = Some(turn_id.clone());
                session.last_turn_diff = None;
            }
        }

        // Forward notifications until this turn completes.
        let mut stop_reason = acp::StopReason::EndTurn;
        let mut turn_failure: Option<acp::Error> = None;
        loop {
            let notif = match rx.recv().await {
                Ok(n) => n,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            };

            let Some(params) = notif.params.as_ref() else {
                continue;
            };

            let notif_thread_id = if notif.method.starts_with("request:") {
                params
                    .get("params")
                    .and_then(|p| p.get("threadId"))
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        params
                            .get("params")
                            .and_then(|p| p.get("thread"))
                            .and_then(|t| t.get("id"))
                            .and_then(|v| v.as_str())
                    })
            } else {
                params.get("threadId").and_then(|v| v.as_str()).or_else(|| {
                    params
                        .get("thread")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                })
            };
            if notif_thread_id != Some(thread_id.as_str()) {
                continue;
            }

            if notif.method.starts_with("request:") {
                self.handle_codex_request_notification(&codex, &request.session_id, &notif)
                    .await?;
                continue;
            }

            if notif.method == "item/mcpToolCall/progress" {
                let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
                let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if !item_id.is_empty() {
                    let tool_call_id_str = Self::tool_call_id_string_for_codex_item(item_id);
                    let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

                    // Ensure a tool call exists for progress updates.
                    let first_seen = {
                        let mut sessions = self.sessions.write().await;
                        let session_id_str: &str = &request.session_id.0;
                        sessions
                            .get_mut(session_id_str)
                            .map(|s| s.known_tool_calls.insert(tool_call_id_str))
                            .unwrap_or(true)
                    };
                    if first_seen {
                        let _ = self
                            .emit_session_update(
                                request.session_id.clone(),
                                acp::SessionUpdate::ToolCall(
                                    acp::ToolCall::new(tool_call_id.clone(), "MCP tool call")
                                        .kind(acp::ToolKind::Fetch)
                                        .status(acp::ToolCallStatus::InProgress),
                                ),
                            )
                            .await;
                    }

                    let title = if message.is_empty() {
                        None
                    } else {
                        Some(format!("MCP: {message}"))
                    };
                    let mut fields = acp::ToolCallUpdateFields::new()
                        .kind(acp::ToolKind::Fetch)
                        .status(acp::ToolCallStatus::InProgress);
                    if let Some(t) = title {
                        fields = fields.title(t);
                    }
                    let _ = self
                        .emit_session_update(
                            request.session_id.clone(),
                            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                tool_call_id,
                                fields,
                            )),
                        )
                        .await;
                }
                continue;
            }

            if notif.method == "turn/diff/updated" {
                let diff = params.get("diff").and_then(|v| v.as_str()).unwrap_or("");
                let turn_id = params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                if !diff.is_empty() && !turn_id.is_empty() {
                    let (should_emit, cwd) = {
                        let mut sessions = self.sessions.write().await;
                        let session_id_str: &str = &request.session_id.0;
                        match sessions.get_mut(session_id_str) {
                            Some(session) => {
                                if session.last_turn_diff.as_deref() == Some(diff) {
                                    (false, session.cwd.clone())
                                } else {
                                    session.last_turn_diff = Some(diff.to_string());
                                    (true, session.cwd.clone())
                                }
                            }
                            None => (false, ".".to_string()),
                        }
                    };

                    if should_emit {
                        let tool_call_id_str = format!("codex-turn-diff:{turn_id}");
                        let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

                        let first_seen = {
                            let mut sessions = self.sessions.write().await;
                            let session_id_str: &str = &request.session_id.0;
                            sessions
                                .get_mut(session_id_str)
                                .map(|s| s.known_tool_calls.insert(tool_call_id_str))
                                .unwrap_or(true)
                        };
                        if first_seen {
                            let _ = self
                                .emit_session_update(
                                    request.session_id.clone(),
                                    acp::SessionUpdate::ToolCall(
                                        acp::ToolCall::new(tool_call_id.clone(), "Diff")
                                            .kind(acp::ToolKind::Edit)
                                            .status(acp::ToolCallStatus::InProgress)
                                            .raw_input(serde_json::Value::String(diff.to_string())),
                                    ),
                                )
                                .await;
                        }

                        let content = Self::diff_content_from_unified_diff(&cwd, diff);
                        let mut fields = acp::ToolCallUpdateFields::new()
                            .kind(acp::ToolKind::Edit)
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_output(serde_json::Value::String(diff.to_string()));
                        if !content.is_empty() {
                            fields = fields.content(content);
                        }
                        let _ = self
                            .emit_session_update(
                                request.session_id.clone(),
                                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                    tool_call_id,
                                    fields,
                                )),
                            )
                            .await;
                    }
                }
                continue;
            }

            if notif.method == "item/started" || notif.method == "item/completed" {
                if let Some(item) = params.get("item").and_then(|v| v.as_object()) {
                    let item_id = item.get("id").and_then(|v| v.as_str());
                    let item_type = item.get("type").and_then(|v| v.as_str());
                    if let (Some(item_id), Some(item_type)) = (item_id, item_type) {
                        match item_type {
                            "commandExecution"
                            | "fileChange"
                            | "mcpToolCall"
                            | "webSearch"
                            | "imageView"
                            | "enteredReviewMode"
                            | "exitedReviewMode"
                            | "contextCompaction"
                            | "collabAgentToolCall" => {}
                            _ => continue,
                        };

                        let item_value = serde_json::Value::Object(item.clone());
                        let (kind, title, computed_status) =
                            Self::tool_call_fields_for_item(item_type, &item_value);

                        let tool_call_id_str = Self::tool_call_id_string_for_codex_item(item_id);
                        let tool_call_id = acp::ToolCallId::new(tool_call_id_str.clone());

                        let first_seen = {
                            let mut sessions = self.sessions.write().await;
                            let session_id_str: &str = &request.session_id.0;
                            sessions
                                .get_mut(session_id_str)
                                .map(|s| s.known_tool_calls.insert(tool_call_id_str.clone()))
                                .unwrap_or(true)
                        };

                        if first_seen && notif.method == "item/started" {
                            let _ = self
                                .emit_session_update(
                                    request.session_id.clone(),
                                    acp::SessionUpdate::ToolCall(
                                        acp::ToolCall::new(tool_call_id.clone(), title)
                                            .kind(kind)
                                            .status(acp::ToolCallStatus::InProgress)
                                            .raw_input(item_value.clone()),
                                    ),
                                )
                                .await;
                        }

                        let status = if notif.method == "item/started" {
                            acp::ToolCallStatus::InProgress
                        } else {
                            computed_status
                        };

                        let mut fields = acp::ToolCallUpdateFields::new()
                            .kind(kind)
                            .status(status)
                            .raw_output(item_value.clone());

                        if item_type == "fileChange" {
                            if let Some(content) = self
                                .diff_content_from_file_change_item(
                                    &request.session_id,
                                    &item_value,
                                )
                                .await
                            {
                                fields = fields.content(content);
                            }
                        }

                        let _ = self
                            .emit_session_update(
                                request.session_id.clone(),
                                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                    tool_call_id,
                                    fields,
                                )),
                            )
                            .await;
                    }
                }
                continue;
            }

            if notif.method == "turn/plan/updated" {
                if let Some(plan) = params.get("plan").and_then(|v| v.as_array()) {
                    let entries = plan
                        .iter()
                        .filter_map(|step| {
                            let content = step.get("step")?.as_str()?.to_string();
                            let status = step.get("status")?.as_str()?;
                            let status = match status {
                                "pending" => acp::PlanEntryStatus::Pending,
                                "inProgress" => acp::PlanEntryStatus::InProgress,
                                "completed" => acp::PlanEntryStatus::Completed,
                                _ => acp::PlanEntryStatus::Pending,
                            };
                            Some(acp::PlanEntry::new(
                                content,
                                acp::PlanEntryPriority::Medium,
                                status,
                            ))
                        })
                        .collect::<Vec<_>>();
                    let _ = self
                        .emit_session_update(
                            request.session_id.clone(),
                            acp::SessionUpdate::Plan(acp::Plan::new(entries)),
                        )
                        .await;
                }
                continue;
            }

            if let Some(update) = convert_codex_notification(&notif) {
                let _ = self
                    .emit_session_update(request.session_id.clone(), update)
                    .await;
            }

            if notif.method == "turn/completed" {
                if let Some(turn) = params.get("turn") {
                    if let Some(turn_id) = turn.get("id").and_then(|v| v.as_str()) {
                        // Best-effort: mark the streamed diff tool call as completed.
                        let tool_call_id =
                            acp::ToolCallId::new(format!("codex-turn-diff:{turn_id}"));
                        let _ = self
                            .emit_session_update(
                                request.session_id.clone(),
                                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                    tool_call_id,
                                    acp::ToolCallUpdateFields::new()
                                        .kind(acp::ToolKind::Edit)
                                        .status(acp::ToolCallStatus::Completed),
                                )),
                            )
                            .await;
                    }
                    if let Some(status) = turn.get("status").and_then(|v| v.as_str()) {
                        match status {
                            "interrupted" => {
                                stop_reason = acp::StopReason::Cancelled;
                            }
                            "failed" => {
                                let err_obj = turn.get("error").cloned();
                                let message = turn
                                    .get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Codex turn failed");
                                let additional_details = turn
                                    .get("error")
                                    .and_then(|e| e.get("additionalDetails"))
                                    .and_then(|v| v.as_str());

                                // Cap additionalDetails so large shell output (e.g. pyenv
                                // rehash errors, captured file contents) doesn't produce a
                                // multi-KB error string. The full payload is still available
                                // in the structured err.data below.
                                const MAX_DETAILS_LEN: usize = 500;
                                let mut full_message = message.to_string();
                                if let Some(details) = additional_details {
                                    full_message.push_str("\n\n");
                                    if details.len() > MAX_DETAILS_LEN {
                                        full_message.push_str(&details[..MAX_DETAILS_LEN]);
                                        full_message.push_str("\u{2026} (truncated)");
                                    } else {
                                        full_message.push_str(details);
                                    }
                                }

                                let mut err = acp::Error::new(
                                    -32603,
                                    format!("Codex turn failed: {full_message}"),
                                );
                                if let Some(obj) = err_obj {
                                    if !obj.is_null() {
                                        err = err.data(obj);
                                    }
                                }
                                turn_failure = Some(err);
                                stop_reason = acp::StopReason::EndTurn;
                            }
                            _ => {
                                stop_reason = acp::StopReason::EndTurn;
                            }
                        }
                    }
                }
                break;
            }
        }

        // Clear active turn
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.current_turn_id = None;
            }
        }

        if let Some(err) = turn_failure {
            return Err(err);
        }

        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, request: acp::CancelNotification) -> Result<(), acp::Error> {
        log::info!(
            "Received cancel request for session: {:?}",
            request.session_id
        );

        let Ok(codex) = self.get_codex(None).await else {
            // If Codex isn't running yet, there can't be an active turn to interrupt.
            return Ok(());
        };

        // Get thread ID and turn ID for this session (best-effort).
        let (thread_id, turn_id) = {
            let sessions = self.sessions.read().await;
            let session_id_str: &str = &request.session_id.0;
            let Some(session) = sessions.get(session_id_str) else {
                return Ok(());
            };
            let Some(turn_id) = session.current_turn_id.clone() else {
                return Ok(());
            };
            (session.thread_id.clone(), turn_id)
        };

        if let Err(e) = codex.turn_interrupt(&thread_id, &turn_id).await {
            log::warn!("Failed to interrupt Codex turn (best-effort): {e}");
        }
        Ok(())
    }

    async fn set_session_mode(
        &self,
        request: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        log::info!("Received set session mode request: {:?}", request.mode_id);
        let mode_id: &str = request.mode_id.0.as_ref();
        let approval_policy = Self::approval_policy_for_mode(mode_id);

        // Update stored session config so subsequent turns inherit it.
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            let session = sessions.get_mut(session_id_str).ok_or_else(|| {
                acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
            })?;
            session.approval_policy = approval_policy;
        }

        // Codex app-server applies turn config when starting a new turn; no immediate RPC needed.
        Ok(acp::SetSessionModeResponse::default())
    }

    async fn set_session_model(
        &self,
        request: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        log::info!(
            "Received set session model request: session={}, model={}",
            request.session_id.0,
            request.model_id.0
        );

        let (model, reasoning_effort) = {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            let session = sessions.get_mut(session_id_str).ok_or_else(|| {
                acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
            })?;
            session.model = Some(request.model_id.0.as_ref().to_string());
            (session.model.clone(), session.reasoning_effort.clone())
        };

        // Clamp reasoning effort to something supported by the newly selected model so we don't
        // persist a value that will later cause a 400 from Codex (e.g. "xhigh" with mini models).
        let codex = self.get_codex(None).await?;
        let config_options = self
            .build_config_options(&codex, model.as_deref(), reasoning_effort.as_deref())
            .await?;
        if let Some(resolved_effort) =
            Self::extract_reasoning_effort_from_config_options(&config_options)
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.reasoning_effort = Some(resolved_effort);
            }
        }

        // Inform the client that config options (and current values) may have changed as a result
        // of selecting a different model.
        let _ = self
            .emit_session_update(
                request.session_id.clone(),
                acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                    config_options,
                )),
            )
            .await;

        Ok(acp::SetSessionModelResponse::default())
    }

    async fn set_session_config_option(
        &self,
        request: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        log::info!(
            "Received set session config option: session={}, config={}, value={}",
            request.session_id.0,
            request.config_id.0,
            request.value.0
        );

        let (model, reasoning_effort) = {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            let session = sessions.get_mut(session_id_str).ok_or_else(|| {
                acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
            })?;

            match request.config_id.0.as_ref() {
                "reasoning_effort" => {
                    session.reasoning_effort = Some(request.value.0.as_ref().to_string());
                }
                _ => {
                    return Err(acp::Error::invalid_params().data(serde_json::Value::String(
                        "Unsupported config option".into(),
                    )));
                }
            }

            (session.model.clone(), session.reasoning_effort.clone())
        };

        let codex = self.get_codex(None).await?;
        let config_options = self
            .build_config_options(&codex, model.as_deref(), reasoning_effort.as_deref())
            .await?;

        if let Some(resolved_effort) =
            Self::extract_reasoning_effort_from_config_options(&config_options)
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.reasoning_effort = Some(resolved_effort);
            }
        }

        // The client doesn't necessarily read the RPC response body; send an update so the UI can
        // reflect clamped values and any option list changes.
        let _ = self
            .emit_session_update(
                request.session_id.clone(),
                acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                    config_options.clone(),
                )),
            )
            .await;

        Ok(acp::SetSessionConfigOptionResponse::new(config_options))
    }

    async fn list_sessions(
        &self,
        request: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        log::info!("Received list sessions request");

        let codex = self.get_codex(None).await?;
        self.ensure_authenticated(&codex).await?;

        let raw = codex
            .thread_list(
                request.cursor.clone(),
                Some(25),
                Some("updated_at".to_string()),
            )
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to list threads: {e}")))?;

        let filter_cwd = request
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let mut sessions = Vec::new();
        if let Some(items) = raw.get("data").and_then(|v| v.as_array()) {
            for t in items {
                let Some(id) = t.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(cwd) = t.get("cwd").and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Some(ref fcwd) = filter_cwd {
                    if cwd != fcwd {
                        continue;
                    }
                }
                let title = t
                    .get("preview")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let updated_at = t
                    .get("updatedAt")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                sessions.push(
                    acp::SessionInfo::new(acp::SessionId::new(id.to_string()), cwd.to_string())
                        .title(title)
                        .updated_at(updated_at),
                );
            }
        }

        let next_cursor = raw
            .get("nextCursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(acp::ListSessionsResponse::new(sessions).next_cursor(next_cursor))
    }

    async fn ext_method(&self, _request: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _request: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

/// Convert Codex notification to ACP SessionUpdate
fn convert_codex_notification(
    notif: &crate::codex::JsonRpcNotification,
) -> Option<acp::SessionUpdate> {
    let params = notif.params.as_ref()?;

    match notif.method.as_str() {
        "item/agentMessage/delta" => {
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let content = acp::ContentChunk::new(delta.to_string().into());
            Some(acp::SessionUpdate::AgentMessageChunk(content))
        }
        "item/reasoning/delta" => {
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let content = acp::ContentChunk::new(delta.to_string().into());
            Some(acp::SessionUpdate::AgentThoughtChunk(content))
        }
        "item/commandExecution/outputDelta" => {
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let content = acp::ContentChunk::new(delta.to_string().into());
            Some(acp::SessionUpdate::AgentMessageChunk(content))
        }
        "item/fileChange/outputDelta" => {
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let content = acp::ContentChunk::new(delta.to_string().into());
            Some(acp::SessionUpdate::AgentMessageChunk(content))
        }
        _ => None,
    }
}

/// Run the ACP server on stdio
pub async fn run_acp_server(_codex_url: &str) -> Result<(), acp::Error> {
    let _ = env_logger::try_init();

    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    // Use LocalSet for non-Send futures
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<AcpOutbound>();

            // Create agent and connection
            let (conn, handle_io) =
                acp::AgentSideConnection::new(CodexAgent::new(tx), outgoing, incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });

            // Background task to send notifications / permission requests
            tokio::task::spawn_local(async move {
                while let Some(msg) = rx.recv().await {
                    match msg {
                        AcpOutbound::SessionNotification(notification) => {
                            if let Err(e) = conn.session_notification(notification).await {
                                log::error!("Failed to send notification: {}", e);
                                break;
                            }
                        }
                        AcpOutbound::RequestPermission {
                            request,
                            response_tx,
                        } => {
                            let res = conn.request_permission(request).await;
                            let _ = response_tx.send(res);
                        }
                    }
                }
            });

            // Run until stdio closes
            handle_io.await
        })
        .await
}
