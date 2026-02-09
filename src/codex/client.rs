// ABOUTME: Stdio client for Codex app-server
// ABOUTME: Spawns codex app-server as subprocess and communicates via stdio

use crate::error::{Error, Result};
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};

use super::types::*;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Pending request awaiting response
struct PendingRequest {
    tx: oneshot::Sender<JsonRpcMessage>,
}

/// Codex app-server client (stdio-based)
pub struct CodexClient {
    /// Request ID counter
    request_id: AtomicI64,
    /// Pending requests awaiting responses
    pending: Arc<RwLock<HashMap<RequestId, PendingRequest>>>,
    /// Channel to send messages to stdin
    tx: mpsc::Sender<String>,
    /// Broadcast channel for notifications
    notifications: broadcast::Sender<JsonRpcNotification>,
    /// Child process handle
    #[allow(dead_code)]
    child: Arc<RwLock<Child>>,
}

impl CodexClient {
    /// Spawn and connect to Codex app-server
    ///
    /// # Arguments
    /// * `_url` - Unused URL parameter (kept for API compatibility)
    /// * `mcp_servers` - Optional MCP server configurations to pass to codex
    pub async fn connect(_url: &str, mcp_servers: Option<Vec<McpServerConfig>>) -> Result<Self> {
        info!("Spawning Codex app-server process");

        let mut cmd = Command::new("codex");
        cmd.arg("app-server");

        // Add MCP server configurations as -c arguments
        if let Some(servers) = mcp_servers {
            for server in servers {
                info!("Adding MCP server '{}' to codex config", server.name);
                for arg in server.to_config_args() {
                    cmd.arg(arg);
                }
            }
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::CodexConnection(format!("Failed to spawn codex: {}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::CodexConnection("Failed to get stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::CodexConnection("Failed to get stdout".to_string()))?;

        let (tx, mut rx) = mpsc::channel::<String>(100);
        let (notif_tx, _) = broadcast::channel::<JsonRpcNotification>(100);

        let pending: Arc<RwLock<HashMap<RequestId, PendingRequest>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                trace!("Sending to Codex: {}", msg);
                if let Err(e) = stdin.write_all(msg.as_bytes()).await {
                    error!("Failed to write to stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    error!("Failed to write newline: {}", e);
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    error!("Failed to flush stdin: {}", e);
                    break;
                }
            }
            debug!("Stdin writer task ended");
        });

        // Spawn reader task
        let pending_clone = Arc::clone(&pending);
        let notif_tx_clone = notif_tx.clone();
        let mut reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        info!("Codex stdout EOF");
                        break;
                    }
                    Ok(_) => {
                        let text = line.trim();
                        if !text.is_empty() {
                            trace!("Received from Codex: {}", text);
                            Self::handle_message(text, &pending_clone, &notif_tx_clone).await;
                        }
                    }
                    Err(e) => {
                        error!("Failed to read from stdout: {}", e);
                        break;
                    }
                }
            }

            // Fail any in-flight requests so callers don't hang forever.
            let mut guard = pending_clone.write().await;
            for (id, pending_req) in guard.drain() {
                let _ = pending_req
                    .tx
                    .send(JsonRpcMessage::Error(JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        error: JsonRpcError {
                            code: -32000,
                            message: "Codex process closed".to_string(),
                            data: None,
                        },
                    }));
            }
            debug!("Stdout reader task ended");
        });

        let client = Self {
            request_id: AtomicI64::new(1),
            pending,
            tx,
            notifications: notif_tx,
            child: Arc::new(RwLock::new(child)),
        };

        // Initialize the connection
        client.initialize().await?;

        Ok(client)
    }

    /// Handle incoming message from stdout
    async fn handle_message(
        text: &str,
        pending: &Arc<RwLock<HashMap<RequestId, PendingRequest>>>,
        notif_tx: &broadcast::Sender<JsonRpcNotification>,
    ) {
        // Parse as JsonRpcMessage which handles all variants
        match serde_json::from_str::<JsonRpcMessage>(text) {
            Ok(JsonRpcMessage::Response(response)) => {
                if matches!(response.id, RequestId::Null) {
                    warn!(
                        "Received JSON-RPC response with null id (dropping): {}",
                        response.jsonrpc
                    );
                    return;
                }
                let mut guard = pending.write().await;
                if let Some(req) = guard.remove(&response.id) {
                    let _ = req.tx.send(JsonRpcMessage::Response(response));
                } else {
                    warn!("Received response for unknown request: {:?}", response.id);
                }
            }
            Ok(JsonRpcMessage::Error(error)) => {
                // JSON-RPC 2.0 allows `id: null` for errors where the request id is unknown.
                // To avoid stranding callers until REQUEST_TIMEOUT, fail all in-flight requests.
                if matches!(error.id, RequestId::Null) {
                    warn!(
                        "Received JSON-RPC error with null id; failing all pending requests: {} {}",
                        error.error.code, error.error.message
                    );
                    let mut guard = pending.write().await;
                    for (id, req) in guard.drain() {
                        let _ = req.tx.send(JsonRpcMessage::Error(JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            error: error.error.clone(),
                        }));
                    }
                    return;
                }
                let mut guard = pending.write().await;
                if let Some(req) = guard.remove(&error.id) {
                    let _ = req.tx.send(JsonRpcMessage::Error(error));
                } else {
                    warn!("Received error for unknown request: {:?}", error.id);
                }
            }
            Ok(JsonRpcMessage::Notification(notification)) => {
                debug!("Received notification: {}", notification.method);
                let _ = notif_tx.send(notification);
            }
            Ok(JsonRpcMessage::Request(request)) => {
                debug!("Received server request: {}", request.method);
                // Server requests (e.g., approval) are forwarded as notifications
                let notif = JsonRpcNotification {
                    jsonrpc: "2.0".to_string(),
                    method: format!("request:{}", request.method),
                    params: Some(serde_json::json!({
                        "id": request.id,
                        "params": request.params
                    })),
                };
                let _ = notif_tx.send(notif);
            }
            Err(e) => {
                if Self::handle_message_fallback(text, pending, notif_tx).await {
                    return;
                }
                warn!("Could not parse message: {} - {}", text, e);
            }
        }
    }

    async fn handle_message_fallback(
        text: &str,
        pending: &Arc<RwLock<HashMap<RequestId, PendingRequest>>>,
        notif_tx: &broadcast::Sender<JsonRpcNotification>,
    ) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return false;
        };
        Self::handle_fallback_value(&value, pending, notif_tx).await
    }

    async fn handle_fallback_value(
        value: &serde_json::Value,
        pending: &Arc<RwLock<HashMap<RequestId, PendingRequest>>>,
        notif_tx: &broadcast::Sender<JsonRpcNotification>,
    ) -> bool {
        // Avoid recursion in async fn by using an explicit stack.
        let mut stack = vec![value];
        let mut any_handled = false;

        while let Some(value) = stack.pop() {
            match value {
                serde_json::Value::Array(items) => {
                    // Preserve original order by pushing in reverse (stack is LIFO).
                    for item in items.iter().rev() {
                        stack.push(item);
                    }
                }
                serde_json::Value::Object(obj) => {
                    // Classify as request/notification/response/error using standard JSON-RPC fields.
                    if let Some(method) = obj.get("method").and_then(|v| v.as_str()) {
                        let params = obj.get("params").cloned();
                        if let Some(id_value) = obj.get("id") {
                            if let Some(id) = Self::parse_request_id(id_value) {
                                // Server requests (e.g., approval) are forwarded as notifications.
                                let notif = JsonRpcNotification {
                                    jsonrpc: "2.0".to_string(),
                                    method: format!("request:{method}"),
                                    params: Some(serde_json::json!({
                                        "id": id,
                                        "params": params
                                    })),
                                };
                                let _ = notif_tx.send(notif);
                                any_handled = true;
                                continue;
                            }
                        }

                        // Notification (no id / not parseable id)
                        let notif = JsonRpcNotification {
                            jsonrpc: "2.0".to_string(),
                            method: method.to_string(),
                            params,
                        };
                        let _ = notif_tx.send(notif);
                        any_handled = true;
                        continue;
                    }

                    if obj.contains_key("result") && obj.contains_key("id") {
                        let Some(id) = obj.get("id").and_then(Self::parse_request_id) else {
                            continue;
                        };
                        if matches!(id, RequestId::Null) {
                            warn!("Received JSON-RPC response with null id (dropping)");
                            any_handled = true;
                            continue;
                        }

                        let response = JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: id.clone(),
                            result: obj
                                .get("result")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        };

                        let mut guard = pending.write().await;
                        if let Some(req) = guard.remove(&id) {
                            let _ = req.tx.send(JsonRpcMessage::Response(response));
                        } else {
                            warn!("Received response for unknown request: {:?}", id);
                        }
                        any_handled = true;
                        continue;
                    }

                    if obj.contains_key("error") && obj.contains_key("id") {
                        let Some(id) = obj.get("id").and_then(Self::parse_request_id) else {
                            continue;
                        };

                        let error_obj = obj.get("error").unwrap_or(&serde_json::Value::Null);
                        let error = JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id: id.clone(),
                            error: Self::parse_error(error_obj),
                        };

                        // Mirror the strict-path behavior for `id: null`.
                        if matches!(id, RequestId::Null) {
                            warn!(
                                "Received JSON-RPC error with null id; failing all pending requests: {} {}",
                                error.error.code, error.error.message
                            );
                            let mut guard = pending.write().await;
                            for (pending_id, req) in guard.drain() {
                                let _ = req.tx.send(JsonRpcMessage::Error(JsonRpcErrorResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id: pending_id,
                                    error: error.error.clone(),
                                }));
                            }
                            any_handled = true;
                            continue;
                        }

                        let mut guard = pending.write().await;
                        if let Some(req) = guard.remove(&id) {
                            let _ = req.tx.send(JsonRpcMessage::Error(error));
                        } else {
                            warn!("Received error for unknown request: {:?}", id);
                        }
                        any_handled = true;
                        continue;
                    }
                }
                _ => {}
            }
        }

        any_handled
    }

    fn parse_request_id(v: &serde_json::Value) -> Option<RequestId> {
        match v {
            serde_json::Value::Null => Some(RequestId::Null),
            serde_json::Value::String(s) => Some(RequestId::String(s.clone())),
            serde_json::Value::Number(n) => n.as_i64().map(RequestId::Number),
            _ => None,
        }
    }

    fn parse_error(v: &serde_json::Value) -> JsonRpcError {
        let serde_json::Value::Object(obj) = v else {
            return JsonRpcError {
                code: -32603,
                message: v.to_string(),
                data: None,
            };
        };

        let code = obj.get("code").and_then(|v| v.as_i64()).unwrap_or(-32603);
        let message = obj
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        let data = obj.get("data").cloned();

        JsonRpcError {
            code,
            message,
            data,
        }
    }

    /// Get next request ID
    fn next_id(&self) -> RequestId {
        RequestId::Number(self.request_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Send a request and wait for response
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let id = self.next_id();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        let json = serde_json::to_string(&request)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.write().await;
            guard.insert(id.clone(), PendingRequest { tx });
        }

        if let Err(e) = self.tx.send(json).await {
            let mut guard = self.pending.write().await;
            guard.remove(&id);
            return Err(Error::CodexConnection(e.to_string()));
        }

        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(_)) => {
                let mut guard = self.pending.write().await;
                guard.remove(&id);
                return Err(Error::CodexConnection("Request cancelled".to_string()));
            }
            Err(_) => {
                let mut guard = self.pending.write().await;
                guard.remove(&id);
                return Err(Error::CodexConnection(format!(
                    "Request timed out after {:?}",
                    REQUEST_TIMEOUT
                )));
            }
        };

        match response {
            JsonRpcMessage::Response(resp) => Ok(resp.result),
            JsonRpcMessage::Error(err) => Err(Error::CodexConnection(format!(
                "Codex error {}: {}",
                err.error.code, err.error.message
            ))),
            _ => Err(Error::CodexConnection(
                "Unexpected response type".to_string(),
            )),
        }
    }

    /// Send a notification (no response expected)
    pub async fn notify(&self, method: &str, params: Option<serde_json::Value>) -> Result<()> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        };
        let json = serde_json::to_string(&notification)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| Error::CodexConnection(e.to_string()))?;
        Ok(())
    }

    /// Subscribe to notifications
    pub fn subscribe(&self) -> broadcast::Receiver<JsonRpcNotification> {
        self.notifications.subscribe()
    }

    /// Initialize connection with Codex
    async fn initialize(&self) -> Result<()> {
        let params = InitializeParams {
            client_info: ClientInfo {
                name: "seren-acp-codex".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("Seren Codex ACP Agent".to_string()),
            },
        };

        let result = self
            .request("initialize", Some(serde_json::to_value(params)?))
            .await?;
        info!("Codex initialized: {:?}", result);

        // Send initialized notification
        self.notify("initialized", None).await?;

        Ok(())
    }

    /// Start a new thread
    pub async fn thread_start(&self, params: ThreadStartParams) -> Result<String> {
        let result = self.thread_start_full(params).await?;
        let thread_id = result
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::CodexConnection("No thread.id in response".to_string()))?;
        Ok(thread_id.to_string())
    }

    pub async fn thread_start_full(&self, params: ThreadStartParams) -> Result<serde_json::Value> {
        self.request("thread/start", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn thread_resume(&self, params: ThreadResumeParams) -> Result<serde_json::Value> {
        self.request("thread/resume", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn thread_read(
        &self,
        thread_id: &str,
        include_turns: bool,
    ) -> Result<serde_json::Value> {
        let params = ThreadReadParams {
            thread_id: thread_id.to_string(),
            include_turns,
        };
        self.request("thread/read", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn thread_list(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        sort_key: Option<String>,
    ) -> Result<serde_json::Value> {
        let params = ThreadListParams {
            cursor,
            limit,
            sort_key,
            archived: Some(false),
        };
        self.request("thread/list", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn model_list(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<serde_json::Value> {
        let params = ModelListParams { cursor, limit };
        self.request("model/list", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn account_read(&self, refresh_token: bool) -> Result<serde_json::Value> {
        let params = GetAccountParams { refresh_token };
        self.request("account/read", Some(serde_json::to_value(&params)?))
            .await
    }

    pub async fn account_login_start(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.request("account/login/start", Some(params)).await
    }

    pub async fn account_login_cancel(&self, login_id: &str) -> Result<()> {
        self.request(
            "account/login/cancel",
            Some(serde_json::json!({ "loginId": login_id })),
        )
        .await?;
        Ok(())
    }

    /// Start a turn - send user input and begin agent processing
    pub async fn turn_start(&self, thread_id: &str, input: Vec<UserInput>) -> Result<String> {
        self.turn_start_with_policy(thread_id, input, None).await
    }

    pub async fn turn_start_with_policy(
        &self,
        thread_id: &str,
        input: Vec<UserInput>,
        approval_policy: Option<ApprovalPolicy>,
    ) -> Result<String> {
        self.turn_start_with_overrides(thread_id, input, approval_policy, None, None, None)
            .await
    }

    pub async fn turn_start_with_overrides(
        &self,
        thread_id: &str,
        input: Vec<UserInput>,
        approval_policy: Option<ApprovalPolicy>,
        model: Option<String>,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<String> {
        let params = TurnStartParams {
            thread_id: thread_id.to_string(),
            input,
            cwd: None,
            approval_policy,
            effort,
            summary,
            model,
        };
        let result = self
            .request("turn/start", Some(serde_json::to_value(&params)?))
            .await?;
        let turn_id = result
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::CodexConnection("No turn.id in response".to_string()))?;
        Ok(turn_id.to_string())
    }

    /// Interrupt current turn
    pub async fn turn_interrupt(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        let params = TurnInterruptParams {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
        };
        self.request("turn/interrupt", Some(serde_json::to_value(&params)?))
            .await?;
        Ok(())
    }

    /// Respond to a server request (approval)
    pub async fn respond(&self, id: RequestId, result: serde_json::Value) -> Result<()> {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        };
        let json = serde_json::to_string(&response)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| Error::CodexConnection(e.to_string()))?;
        Ok(())
    }

    pub async fn respond_error(
        &self,
        id: RequestId,
        code: i64,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let response = JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
                data,
            },
        };
        let json = serde_json::to_string(&response)?;
        self.tx
            .send(json)
            .await
            .map_err(|e| Error::CodexConnection(e.to_string()))?;
        Ok(())
    }
}
