//! Axum shell around one Echo environment per session.
//!
//! `POST /mcp` creates the session the official `EchoEnv` client asks for.
//! `GET /ws` attaches that session, or creates one when `session_id` is
//! omitted. The socket's task owns the attachment. When it finishes, the
//! session and its `MemoryEngine` are dropped.
//!
//! That cleanup is a property of this table, not of the kernel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::env::{EchoEnvironment, EnvError, Environment};
use crate::kernel::{Body, StepAction};
use crate::protocol::{self, Incoming, WireError};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub max_sessions: usize,
    pub max_message_bytes: usize,
    /// Sessions that never attach, or whose socket is already gone, are
    /// removed after this long. `None` disables the sweep.
    pub idle_timeout: Option<Duration>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_sessions: 8,
            max_message_bytes: 16 * 1024 * 1024,
            idle_timeout: Some(Duration::from_secs(120)),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    sessions: Arc<SessionTable>,
    config: Arc<ServerConfig>,
}

struct SessionTable {
    inner: Mutex<HashMap<String, Arc<Slot>>>,
    max_sessions: usize,
    idle_timeout: Option<Duration>,
}

struct Slot {
    env: EchoEnvironment,
    attached: AtomicBool,
    pending_close: AtomicBool,
    last_activity: Mutex<Instant>,
}

enum SessionError {
    Capacity { active: usize, max: usize },
    Unknown(String),
    AlreadyAttached(String),
    Closing(String),
}

enum CloseOutcome {
    Closed,
    Closing,
}

impl SessionTable {
    fn new(config: &ServerConfig) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_sessions: config.max_sessions,
            idle_timeout: config.idle_timeout,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    fn sweep_locked(&self, sessions: &mut HashMap<String, Arc<Slot>>) {
        let Some(idle) = self.idle_timeout else {
            return;
        };
        let now = Instant::now();
        sessions.retain(|_, slot| {
            if slot.attached.load(Ordering::SeqCst) {
                return true;
            }
            let last = match slot.last_activity.lock() {
                Ok(guard) => *guard,
                Err(poison) => *poison.into_inner(),
            };
            now.saturating_duration_since(last) < idle
        });
    }

    fn sweep(&self) {
        let mut sessions = self.lock();
        self.sweep_locked(&mut sessions);
    }

    fn insert(&self, attached: bool) -> Result<(String, Arc<Slot>), SessionError> {
        let mut sessions = self.lock();
        self.sweep_locked(&mut sessions);
        if sessions.len() >= self.max_sessions {
            return Err(SessionError::Capacity {
                active: sessions.len(),
                max: self.max_sessions,
            });
        }
        let id = Uuid::new_v4().to_string();
        let episode_id = Uuid::new_v4().to_string();
        let slot = Arc::new(Slot {
            env: EchoEnvironment::open(episode_id),
            attached: AtomicBool::new(attached),
            pending_close: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
        });
        sessions.insert(id.clone(), slot.clone());
        Ok((id, slot))
    }

    fn open(&self) -> Result<String, SessionError> {
        self.insert(false).map(|(id, _)| id)
    }

    fn open_and_attach(&self) -> Result<(String, Arc<Slot>), SessionError> {
        self.insert(true)
    }

    fn attach(&self, id: &str) -> Result<Arc<Slot>, SessionError> {
        let sessions = self.lock();
        let slot = sessions
            .get(id)
            .ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        if slot.pending_close.load(Ordering::SeqCst) {
            return Err(SessionError::Closing(id.to_string()));
        }
        if slot.attached.swap(true, Ordering::SeqCst) {
            return Err(SessionError::AlreadyAttached(id.to_string()));
        }
        touch(slot);
        Ok(slot.clone())
    }

    fn get(&self, id: &str) -> Result<Arc<Slot>, SessionError> {
        let sessions = self.lock();
        let slot = sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        touch(&slot);
        Ok(slot)
    }

    fn destroy(&self, id: &str) {
        let mut sessions = self.lock();
        sessions.remove(id);
    }

    fn close_http(&self, id: &str) -> Result<CloseOutcome, SessionError> {
        let mut sessions = self.lock();
        let slot = sessions
            .get(id)
            .ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        if slot.attached.load(Ordering::SeqCst) {
            slot.pending_close.store(true, Ordering::SeqCst);
            Ok(CloseOutcome::Closing)
        } else {
            sessions.remove(id);
            Ok(CloseOutcome::Closed)
        }
    }
}

fn touch(slot: &Slot) {
    let mut guard = match slot.last_activity.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    *guard = Instant::now();
}

impl AppState {
    pub fn new(config: ServerConfig) -> Self {
        let sessions = Arc::new(SessionTable::new(&config));
        Self {
            sessions,
            config: Arc::new(config),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/metadata", get(metadata))
            .route("/schema", get(schema))
            .route("/mcp", post(mcp))
            .route("/ws", get(ws))
            .with_state(self.clone())
    }
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    let sweeper = state.clone();
    let sweep = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            sweeper.sessions.sweep();
        }
    });
    let result = axum::serve(listener, state.router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    sweep.abort();
    result
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "healthy" }))
}

async fn metadata() -> Json<Value> {
    Json(protocol::metadata_document())
}

async fn schema() -> Json<Value> {
    Json(protocol::schema_document())
}

#[derive(Deserialize)]
struct WsQuery {
    session_id: Option<String>,
}

async fn ws(
    upgrade: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let max = state.config.max_message_bytes;
    upgrade
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| handle_socket(socket, state, query.session_id))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, session_id: Option<String>) {
    let attached = match session_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => match state.sessions.attach(id) {
            Ok(slot) => Some((id.to_string(), slot)),
            Err(error) => {
                let _ = send_text(&mut socket, &session_error_frame(&error)).await;
                close_socket(&mut socket).await;
                return;
            }
        },
        None => match state.sessions.open_and_attach() {
            Ok(pair) => Some(pair),
            Err(error) => {
                let _ = send_text(&mut socket, &session_error_frame(&error)).await;
                close_socket(&mut socket).await;
                return;
            }
        },
    };
    let Some((id, slot)) = attached else {
        return;
    };
    message_loop(&mut socket, &slot).await;
    state.sessions.destroy(&id);
    close_socket(&mut socket).await;
}

async fn message_loop(socket: &mut WebSocket, slot: &Slot) {
    loop {
        let message = match socket.recv().await {
            Some(Ok(message)) => message,
            _ => break,
        };
        match message {
            Message::Text(text) => {
                touch(slot);
                match dispatch(&slot.env, text.as_str()) {
                    Dispatch::Reply(frame) => {
                        if send_text(socket, &frame).await.is_err() {
                            break;
                        }
                    }
                    Dispatch::Close => break,
                }
            }
            Message::Binary(_) => {
                let frame = protocol::error_frame(&WireError::new(
                    "INVALID_JSON",
                    "Invalid JSON: binary frame",
                ));
                if send_text(socket, &frame).await.is_err() {
                    break;
                }
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }
}

enum Dispatch {
    Reply(String),
    Close,
}

fn dispatch(env: &EchoEnvironment, text: &str) -> Dispatch {
    let incoming = match protocol::parse_incoming(text) {
        Ok(incoming) => incoming,
        Err(error) => return Dispatch::Reply(protocol::error_frame(&error)),
    };
    match incoming {
        Incoming::Close => Dispatch::Close,
        Incoming::State => match env.state() {
            Ok(state) => {
                Dispatch::Reply(protocol::state_frame(&state.episode_id, state.step_count))
            }
            Err(error) => Dispatch::Reply(execution_frame(&error)),
        },
        Incoming::Reset { seed, episode_id } => {
            let episode_id = episode_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            match env.reset(seed, episode_id) {
                Ok(outcome) => Dispatch::Reply(protocol::observation_frame(&outcome)),
                Err(error) => Dispatch::Reply(execution_frame(&error)),
            }
        }
        Incoming::Step(action) => match env.step(action) {
            Ok(outcome) => Dispatch::Reply(protocol::observation_frame(&outcome)),
            Err(error) => Dispatch::Reply(execution_frame(&error)),
        },
    }
}

fn execution_frame(error: &EnvError) -> String {
    protocol::error_frame(&WireError::new("EXECUTION_ERROR", error.to_string()))
}

fn session_error_frame(error: &SessionError) -> String {
    match error {
        SessionError::Capacity { active, max } => protocol::error_frame(
            &WireError::new(
                "CAPACITY_REACHED",
                format!("Server at capacity: {active}/{max} sessions active. Cannot accept new connections."),
            )
            .with_extra(vec![
                ("active_sessions", json!(active)),
                ("max_sessions", json!(max)),
            ]),
        ),
        SessionError::Unknown(id) => {
            protocol::error_frame(&WireError::new("SESSION_ERROR", format!("Unknown session_id: {id}")))
        }
        SessionError::AlreadyAttached(id) => protocol::error_frame(&WireError::new(
            "SESSION_ERROR",
            format!("Session {id} already has an attached WebSocket"),
        )),
        SessionError::Closing(id) => {
            protocol::error_frame(&WireError::new("SESSION_ERROR", format!("Session {id} is closing")))
        }
    }
}

async fn send_text(socket: &mut WebSocket, text: &str) -> Result<(), axum::Error> {
    socket.send(Message::Text(text.to_string().into())).await
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

async fn mcp(State(state): State<AppState>, body: axum::body::Bytes) -> impl IntoResponse {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return rpc_response(rpc_error(Value::Null, -32700, "Parse error", None)),
    };
    let obj = match request.as_object() {
        Some(obj) => obj,
        None => return rpc_response(rpc_error(Value::Null, -32600, "Invalid Request", None)),
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !obj.get("method").is_some() {
        return rpc_response(rpc_error(id, -32600, "Invalid Request", None));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return rpc_response(rpc_error(id, -32600, "Invalid Request", None));
    };
    let params = match obj.get("params") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return rpc_response(rpc_error(id, -32602, "Params must be an object", None)),
    };
    let payload = match method {
        "openenv/session/create" => match state.sessions.open() {
            Ok(session_id) => rpc_ok(id, json!({ "session_id": session_id })),
            Err(error) => session_rpc_error(id, &error),
        },
        "openenv/session/close" => match params.get("session_id").and_then(Value::as_str) {
            None => rpc_error(
                id,
                -32602,
                "Invalid params - 'session_id' is required",
                None,
            ),
            Some(session_id) => match state.sessions.close_http(session_id) {
                Ok(CloseOutcome::Closed) => {
                    rpc_ok(id, json!({ "session_id": session_id, "closed": true }))
                }
                Ok(CloseOutcome::Closing) => rpc_ok(
                    id,
                    json!({ "session_id": session_id, "closed": false, "closing": true }),
                ),
                Err(error) => session_rpc_error(id, &error),
            },
        },
        "tools/list" => match session_from_params(&state, &params) {
            Ok(_slot) => rpc_ok(id, json!({ "tools": protocol::http_tools() })),
            Err(error) => session_rpc_error(id, &error),
        },
        "tools/call" => match call_tool(&state, id.clone(), &params) {
            Ok(value) => rpc_ok(id, value),
            Err(error) => error,
        },
        other => rpc_error(id, -32601, format!("Method not found: {other}"), None),
    };
    rpc_response(payload)
}

fn session_from_params(
    state: &AppState,
    params: &Map<String, Value>,
) -> Result<Arc<Slot>, SessionError> {
    let Some(id) = params.get("session_id").and_then(Value::as_str) else {
        return Err(SessionError::Unknown(String::new()));
    };
    if id.is_empty() {
        return Err(SessionError::Unknown(id.to_string()));
    }
    state.sessions.get(id)
}

fn call_tool(state: &AppState, id: Value, params: &Map<String, Value>) -> Result<Value, Value> {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(name) => name,
        None => return Err(rpc_error(id, -32602, "Missing 'name' in params", None)),
    };
    let arguments = match params.get("arguments") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err(rpc_error(
                id,
                -32602,
                "Invalid params: arguments must be an object",
                None,
            ));
        }
    };
    let slot = match session_from_params(state, params) {
        Ok(slot) => slot,
        Err(error) => return Err(session_rpc_error(id, &error)),
    };
    let outcome = match slot
        .env
        .invoke(StepAction::Call(protocol::tool_call(name, &arguments)))
    {
        Ok(outcome) => outcome,
        Err(error) => return Err(rpc_error(id, -32603, error.to_string(), None)),
    };
    match outcome.body {
        Body::Called { output, .. } => Ok(tool_result_value(&output)),
        Body::Failed {
            error_type,
            message,
            ..
        } => {
            let code = match error_type {
                crate::kernel::ToolErrorType::NotFound
                | crate::kernel::ToolErrorType::InvalidArgs => -32602,
                crate::kernel::ToolErrorType::Execution => -32603,
            };
            Err(rpc_error(id, code, message, None))
        }
        Body::Ready | Body::Tools => Err(rpc_error(
            id,
            -32603,
            "tool call did not return a tool result",
            None,
        )),
    }
}

/// The production HTTP client unwraps `result.data` when the payload is a
/// FastMCP `CallToolResult`. Match that shape.
fn tool_result_value(output: &Value) -> Value {
    let text = match output {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structured_content": { "result": output },
        "meta": null,
        "data": output,
        "is_error": false
    })
}

fn rpc_response(payload: Value) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        payload.to_string(),
    )
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message.into() });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

fn session_rpc_error(id: Value, error: &SessionError) -> Value {
    match error {
        SessionError::Capacity { active, max } => rpc_error(
            id,
            -32000,
            format!(
                "Server at capacity: {active}/{max} sessions active. Cannot accept new connections."
            ),
            Some(json!({ "active_sessions": active, "max_sessions": max })),
        ),
        SessionError::Unknown(session_id) if session_id.is_empty() => rpc_error(
            id,
            -32602,
            "Invalid params - 'session_id' is required",
            None,
        ),
        SessionError::Unknown(session_id) => rpc_error(
            id,
            -32602,
            format!("Unknown session_id: {session_id}"),
            None,
        ),
        SessionError::AlreadyAttached(session_id) | SessionError::Closing(session_id) => rpc_error(
            id,
            -32600,
            format!("Session {session_id} is not available"),
            None,
        ),
    }
}
