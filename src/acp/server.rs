// ABOUTME: ACP server implementation using official rust-sdk trait API
// ABOUTME: Implements acp::Agent trait to bridge ACP to Codex app-server

use crate::codex::{ApprovalPolicy, CodexClient, McpServerConfig, SandboxMode, ThreadStartParams, UserInput};
use agent_client_protocol::{self as acp, Client as _};
use std::collections::{HashMap, HashSet};
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
    /// Next session ID counter
    next_session_id: RwLock<u64>,
}

impl CodexAgent {
    /// Create a new Codex agent
    fn new(acp_tx: mpsc::UnboundedSender<AcpOutbound>) -> Self {
        Self {
            acp_tx,
            codex: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
            next_session_id: RwLock::new(0),
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

    fn approval_policy_for_mode(mode_id: &str) -> ApprovalPolicy {
        match mode_id {
            // "Ask before running tools"
            "ask" => ApprovalPolicy::UnlessTrusted,
            // "Auto-approve safe operations"
            "auto" => ApprovalPolicy::OnFailure,
            _ => ApprovalPolicy::UnlessTrusted,
        }
    }

    fn session_modes() -> acp::SessionModeState {
        let ask_mode =
            acp::SessionMode::new("ask", "Ask").description("Ask before running tools".to_string());
        let auto_mode = acp::SessionMode::new("auto", "Auto")
            .description("Auto-approve safe operations".to_string());

        acp::SessionModeState::new("ask", vec![ask_mode, auto_mode])
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

        // Build capabilities
        let prompt_caps = acp::PromptCapabilities::new().embedded_context(true);
        let agent_caps = acp::AgentCapabilities::new().prompt_capabilities(prompt_caps);

        // Build agent info
        let agent_info = acp::Implementation::new("seren-acp-codex", env!("CARGO_PKG_VERSION"))
            .title("Codex Agent".to_string());

        Ok(acp::InitializeResponse::new(request.protocol_version)
            .agent_capabilities(agent_caps)
            .agent_info(agent_info))
    }

    async fn authenticate(
        &self,
        _request: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        log::info!("Received authenticate request");
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        log::info!("Received new session request for cwd: {:?}", request.cwd);
        log::info!(
            "MCP servers from client: {}",
            request.mcp_servers.len()
        );

        // Convert ACP MCP servers to Codex config format
        let mcp_servers = convert_acp_mcp_to_codex(&request.mcp_servers);

        let codex = self.get_codex(mcp_servers).await?;

        // Start a Codex thread
        let cwd = request.cwd.to_string_lossy().to_string();
        let approval_policy = Self::approval_policy_for_mode("ask");
        let thread_params = ThreadStartParams {
            cwd: Some(cwd.clone()),
            model: None,
            model_provider: None,
            approval_policy: Some(approval_policy),
            sandbox: Some(SandboxMode::WorkspaceWrite),
            config: None,
            base_instructions: None,
            developer_instructions: None,
        };

        let thread_id = codex
            .thread_start(thread_params)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to start Codex thread: {}", e)))?;

        log::info!("Created Codex thread {} for cwd {}", thread_id, cwd);

        // Generate session ID
        let session_id = {
            let mut counter = self.next_session_id.write().await;
            let id = *counter;
            *counter += 1;
            format!("session-{}", id)
        };

        // Store session mapping
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionData {
                    thread_id,
                    current_turn_id: None,
                    cwd,
                    approval_policy,
                    known_tool_calls: HashSet::new(),
                },
            );
        }

        Ok(acp::NewSessionResponse::new(session_id).modes(Self::session_modes()))
    }

    async fn load_session(
        &self,
        _request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        log::info!("Received load session request");
        // Codex doesn't support loading existing sessions
        Err(acp::Error::new(-32603, "Session loading not supported"))
    }

    async fn prompt(&self, request: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        log::info!(
            "Received prompt request for session: {:?}",
            request.session_id
        );

        let codex = self.get_codex(None).await?;

        // Get thread ID and session config for this session
        let (thread_id, approval_policy) = {
            let sessions = self.sessions.read().await;
            let session_id_str: &str = &request.session_id.0;
            sessions
                .get(session_id_str)
                .map(|s| (s.thread_id.clone(), s.approval_policy))
                .ok_or_else(|| {
                    acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
                })?
        };

        // Extract prompt text from content blocks and convert to UserInput
        let user_input: Vec<UserInput> = request
            .prompt
            .iter()
            .filter_map(|block| {
                if let acp::ContentBlock::Text(text) = block {
                    Some(UserInput::Text {
                        text: text.text.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        if user_input.is_empty() {
            return Err(acp::Error::new(-32602, "No text content in prompt"));
        }

        log::debug!(
            "Sending prompt to Codex thread {}: {:?}",
            thread_id,
            user_input
        );

        // Subscribe to notifications before sending message, so we don't miss early deltas.
        let mut rx = codex.subscribe();

        // Start turn with user input
        let turn_id = codex
            .turn_start_with_policy(&thread_id, user_input, Some(approval_policy))
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to start turn: {}", e)))?;

        // Store the turn ID for potential cancellation
        {
            let mut sessions = self.sessions.write().await;
            let session_id_str: &str = &request.session_id.0;
            if let Some(session) = sessions.get_mut(session_id_str) {
                session.current_turn_id = Some(turn_id.clone());
            }
        }

        // Forward notifications until this turn completes.
        let mut stop_reason = acp::StopReason::EndTurn;
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

            if notif.method == "item/started" || notif.method == "item/completed" {
                if let Some(item) = params.get("item").and_then(|v| v.as_object()) {
                    let item_id = item.get("id").and_then(|v| v.as_str());
                    let item_type = item.get("type").and_then(|v| v.as_str());
                    if let (Some(item_id), Some(item_type)) = (item_id, item_type) {
                        let (kind, title) = match item_type {
                            "commandExecution" => {
                                let command =
                                    item.get("command").and_then(|v| v.as_str()).unwrap_or("");
                                let title = if command.is_empty() {
                                    "Run command".to_string()
                                } else {
                                    format!("Run command: {command}")
                                };
                                (acp::ToolKind::Execute, title)
                            }
                            "fileChange" => (acp::ToolKind::Edit, "Apply file changes".to_string()),
                            _ => continue,
                        };

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
                                            .raw_input(serde_json::Value::Object(item.clone())),
                                    ),
                                )
                                .await;
                        }

                        let status = match notif.method.as_str() {
                            "item/started" => acp::ToolCallStatus::InProgress,
                            "item/completed" => acp::ToolCallStatus::Completed,
                            _ => acp::ToolCallStatus::Completed,
                        };
                        let _ = self
                            .emit_session_update(
                                request.session_id.clone(),
                                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                    tool_call_id,
                                    acp::ToolCallUpdateFields::new()
                                        .status(status)
                                        .raw_output(serde_json::Value::Object(item.clone())),
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
                    if let Some(status) = turn.get("status").and_then(|v| v.as_str()) {
                        stop_reason = match status {
                            "interrupted" => acp::StopReason::Cancelled,
                            _ => acp::StopReason::EndTurn,
                        };
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

        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, request: acp::CancelNotification) -> Result<(), acp::Error> {
        log::info!(
            "Received cancel request for session: {:?}",
            request.session_id
        );

        let codex = self.get_codex(None).await?;

        // Get thread ID and turn ID for this session
        let (thread_id, turn_id) = {
            let sessions = self.sessions.read().await;
            let session_id_str: &str = &request.session_id.0;
            let session = sessions.get(session_id_str).ok_or_else(|| {
                acp::Error::new(-32603, format!("Session not found: {}", session_id_str))
            })?;

            let turn_id = session
                .current_turn_id
                .clone()
                .ok_or_else(|| acp::Error::new(-32603, "No active turn to cancel"))?;

            (session.thread_id.clone(), turn_id)
        };

        codex
            .turn_interrupt(&thread_id, &turn_id)
            .await
            .map_err(|e| acp::Error::new(-32603, format!("Failed to cancel: {}", e)))?;

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
