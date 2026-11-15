//! Axum shell: OpenEnv routes, the session table, and one [`Session`] per client.
//!
//! `POST /mcp` creates the session that production OpenEnv clients ask for.
//! `GET /ws` attaches that session, or creates one when `session_id` is
//! omitted. The socket's task owns the attachment. When it finishes, the
//! session and its engine are dropped. That cleanup is a property of this
//! table, not of any kernel.

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

use crate::env::{Environment, ToolError, WireError};
use crate::session::{Dispatch, Session, ToolCallError};
use crate::wire;

/// Limits that apply to every environment.
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

/// An OpenEnv server for one environment.
pub struct Server<E: Environment> {
    env: Arc<E>,
    sessions: Arc<SessionTable<E>>,
    config: Arc<ServerConfig>,
}

impl<E: Environment> Clone for Server<E> {
    fn clone(&self) -> Self {
        Self {
            env: self.env.clone(),
            sessions: self.sessions.clone(),
            config: self.config.clone(),
        }
    }
}

impl<E: Environment> Server<E> {
    pub fn new(env: E, config: ServerConfig) -> Self {
        let env = Arc::new(env);
        Self {
            sessions: Arc::new(SessionTable::new(env.clone(), &config)),
            env,
            config: Arc::new(config),
        }
    }

    pub fn environment(&self) -> &E {
        &self.env
    }

    /// The OpenEnv routes. Merge them into a larger Axum app if you need to.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/metadata", get(metadata::<E>))
            .route("/schema", get(schema::<E>))
            .route("/mcp", post(mcp::<E>))
            .route("/ws", get(ws::<E>))
            .with_state(self.clone())
    }

    /// Bind `addr`, serve in a background task, and return the bound address.
    /// Meant for tests: `"127.0.0.1:0"` picks a free port. There is no idle
    /// sweep and no shutdown signal; the task ends with the runtime.
    pub async fn spawn(self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let router = self.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(local)
    }

    /// Serve until Ctrl-C. Idle sessions are swept every 30 seconds.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let sweeper = self.sessions.clone();
        let sweep = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                sweeper.sweep();
            }
        });
        let result = axum::serve(listener, self.router())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await;
        sweep.abort();
        result
    }
}

struct SessionTable<E: Environment> {
    env: Arc<E>,
    inner: Mutex<HashMap<String, Arc<Slot<E>>>>,
    max_sessions: usize,
    idle_timeout: Option<Duration>,
}

struct Slot<E: Environment> {
    session: Session<E>,
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

impl<E: Environment> SessionTable<E> {
    fn new(env: Arc<E>, config: &ServerConfig) -> Self {
        Self {
            env,
            inner: Mutex::new(HashMap::new()),
            max_sessions: config.max_sessions,
            idle_timeout: config.idle_timeout,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Slot<E>>>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    fn sweep_locked(&self, sessions: &mut HashMap<String, Arc<Slot<E>>>) {
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

    fn insert(&self, attached: bool) -> Result<(String, Arc<Slot<E>>), SessionError> {
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
            session: Session::open(self.env.clone(), &episode_id),
            attached: AtomicBool::new(attached),
            pending_close: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
        });
        sessions.insert(id.clone(), slot.clone());
        Ok((id, slot))
    }

    fn attach(&self, id: &str) -> Result<Arc<Slot<E>>, SessionError> {
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

    fn get(&self, id: &str) -> Result<Arc<Slot<E>>, SessionError> {
        let sessions = self.lock();
        let slot = sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::Unknown(id.to_string()))?;
        touch(&slot);
        Ok(slot)
    }

    fn destroy(&self, id: &str) {
        self.lock().remove(id);
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

fn touch<E: Environment>(slot: &Slot<E>) {
    let mut guard = match slot.last_activity.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    *guard = Instant::now();
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "healthy" }))
}

async fn metadata<E: Environment>(State(server): State<Server<E>>) -> Json<Value> {
    Json(server.env.metadata().to_json())
}

async fn schema<E: Environment>(State(server): State<Server<E>>) -> Json<Value> {
    Json(server.env.schema())
}

#[derive(Deserialize)]
struct WsQuery {
    session_id: Option<String>,
}

async fn ws<E: Environment>(
    upgrade: WebSocketUpgrade,
    State(server): State<Server<E>>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let max = server.config.max_message_bytes;
    upgrade
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| handle_socket(socket, server, query.session_id))
}

async fn handle_socket<E: Environment>(
    mut socket: WebSocket,
    server: Server<E>,
    session_id: Option<String>,
) {
    let attached = match session_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => server
            .sessions
            .attach(id)
            .map(|slot| (id.to_string(), slot)),
        None => server.sessions.insert(true),
    };
    let (id, slot) = match attached {
        Ok(pair) => pair,
        Err(error) => {
            let _ = send_text(&mut socket, &session_error_frame(&error)).await;
            close_socket(&mut socket).await;
            return;
        }
    };
    message_loop(&mut socket, &slot).await;
    server.sessions.destroy(&id);
    close_socket(&mut socket).await;
}

async fn message_loop<E: Environment>(socket: &mut WebSocket, slot: &Slot<E>) {
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Text(text) => {
                touch(slot);
                match slot.session.dispatch(text.as_str()) {
                    Dispatch::Reply(frame) => {
                        if send_text(socket, &frame).await.is_err() {
                            break;
                        }
                    }
                    Dispatch::Close => break,
                }
            }
            Message::Binary(_) => {
                let frame = wire::error_frame(&WireError::new(
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

fn session_error_frame(error: &SessionError) -> String {
    let error = match error {
        SessionError::Capacity { active, max } => WireError::new(
            "CAPACITY_REACHED",
            format!(
                "Server at capacity: {active}/{max} sessions active. Cannot accept new connections."
            ),
        )
        .with_extra(vec![
            ("active_sessions", json!(active)),
            ("max_sessions", json!(max)),
        ]),
        SessionError::Unknown(id) => {
            WireError::new("SESSION_ERROR", format!("Unknown session_id: {id}"))
        }
        SessionError::AlreadyAttached(id) => WireError::new(
            "SESSION_ERROR",
            format!("Session {id} already has an attached WebSocket"),
        ),
        SessionError::Closing(id) => {
            WireError::new("SESSION_ERROR", format!("Session {id} is closing"))
        }
    };
    wire::error_frame(&error)
}

async fn send_text(socket: &mut WebSocket, text: &str) -> Result<(), axum::Error> {
    socket.send(Message::Text(text.to_string().into())).await
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

async fn mcp<E: Environment>(
    State(server): State<Server<E>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return rpc_response(rpc_error(Value::Null, -32700, "Parse error", None)),
    };
    let Some(obj) = request.as_object() else {
        return rpc_response(rpc_error(Value::Null, -32600, "Invalid Request", None));
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
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
        "openenv/session/create" => match server.sessions.insert(false) {
            Ok((session_id, _)) => rpc_ok(id, json!({ "session_id": session_id })),
            Err(error) => session_rpc_error(id, &error),
        },
        "openenv/session/close" => match params.get("session_id").and_then(Value::as_str) {
            None => rpc_error(
                id,
                -32602,
                "Invalid params - 'session_id' is required",
                None,
            ),
            Some(session_id) => match server.sessions.close_http(session_id) {
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
        "tools/list" => match session_from_params(&server, &params) {
            Ok(_) => {
                let tools: Vec<Value> = server
                    .env
                    .tools()
                    .into_iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.input_schema,
                        })
                    })
                    .collect();
                rpc_ok(id, json!({ "tools": tools }))
            }
            Err(error) => session_rpc_error(id, &error),
        },
        "tools/call" => match call_tool(&server, id.clone(), &params) {
            Ok(value) => rpc_ok(id, value),
            Err(error) => error,
        },
        other => rpc_error(id, -32601, format!("Method not found: {other}"), None),
    };
    rpc_response(payload)
}

fn session_from_params<E: Environment>(
    server: &Server<E>,
    params: &Map<String, Value>,
) -> Result<Arc<Slot<E>>, SessionError> {
    match params.get("session_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => server.sessions.get(id),
        _ => Err(SessionError::Unknown(String::new())),
    }
}

fn call_tool<E: Environment>(
    server: &Server<E>,
    id: Value,
    params: &Map<String, Value>,
) -> Result<Value, Value> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(rpc_error(id, -32602, "Missing 'name' in params", None));
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
    let slot = session_from_params(server, params)
        .map_err(|error| session_rpc_error(id.clone(), &error))?;
    match slot.session.call_tool(name, &arguments) {
        Ok(output) => Ok(tool_result_value(&output)),
        Err(ToolCallError::NotFound(name)) => {
            Err(rpc_error(id, -32602, format!("Unknown tool: {name}"), None))
        }
        Err(ToolCallError::Tool(ToolError::InvalidArgs(message))) => {
            Err(rpc_error(id, -32602, message, None))
        }
        Err(ToolCallError::Tool(ToolError::Execution(message))) => {
            Err(rpc_error(id, -32603, message, None))
        }
        Err(ToolCallError::Refused(error)) => Err(rpc_error(id, -32603, error.message, None)),
    }
}

/// The production HTTP client unwraps `result.data` when the payload is a
/// FastMCP `CallToolResult`. Match that shape.
pub fn tool_result_value(output: &Value) -> Value {
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
