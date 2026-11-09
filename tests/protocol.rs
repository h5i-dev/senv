#![allow(clippy::unwrap_used, clippy::panic)]
//! WebSocket and HTTP coverage against a real Axum listener.
//!
//! The official Python client is exercised by `scripts/echo_compat.sh`.
//! These tests cover the same routes, plus malformed frames, capacity,
//! reconnect and session isolation.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use senv::{AppState, EnvKind, ServerConfig};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[allow(clippy::unwrap_used)]
#[tokio::test]
async fn echo_reset_step_state_isolation_and_malformed_frames() {
    let base = spawn(4, Some(Duration::from_secs(30))).await;
    let client = reqwest::Client::new();

    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "healthy");

    let left = create_session(&client, &base).await;
    let right = create_session(&client, &base).await;
    assert_ne!(left, right);

    let mut left_ws = connect(&base, &left).await;
    let mut right_ws = connect(&base, &right).await;

    let reset = roundtrip(
        &mut left_ws,
        &json!({"type": "reset", "data": {"episode_id": "left", "seed": 1}}).to_string(),
    )
    .await;
    assert_eq!(reset["type"], "observation");
    assert_eq!(
        reset["data"]["metadata"]["message"],
        "Echo environment ready!"
    );
    assert_eq!(reset["data"]["reward"], 0.0);
    assert_eq!(reset["data"]["done"], false);

    let step = roundtrip(
        &mut left_ws,
        &json!({
            "type": "step",
            "data": {
                "type": "call_tool",
                "tool_name": "echo_with_length",
                "arguments": {"message": "héllo"}
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        step["data"]["observation"]["result"]["data"]["message"],
        "héllo"
    );
    assert_eq!(step["data"]["observation"]["result"]["data"]["length"], 5);
    assert!(step["data"]["reward"].is_null());
    assert_eq!(step["data"]["done"], false);

    let left_state = roundtrip(&mut left_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(left_state["data"]["episode_id"], "left");
    assert_eq!(left_state["data"]["step_count"], 1);

    let right_reset = roundtrip(
        &mut right_ws,
        &json!({"type": "reset", "data": {"episode_id": "right"}}).to_string(),
    )
    .await;
    assert_eq!(right_reset["data"]["metadata"]["status"], "ready");
    let right_state = roundtrip(&mut right_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(right_state["data"]["episode_id"], "right");
    assert_eq!(right_state["data"]["step_count"], 0);

    let bad_json = roundtrip(&mut left_ws, "{").await;
    assert_eq!(bad_json["data"]["code"], "INVALID_JSON");
    let unknown = roundtrip(&mut left_ws, &json!({"type": "nope"}).to_string()).await;
    assert_eq!(unknown["data"]["code"], "UNKNOWN_TYPE");
    let missing = roundtrip(&mut left_ws, &json!({"type": "step"}).to_string()).await;
    assert_eq!(missing["data"]["code"], "VALIDATION_ERROR");
    let after = roundtrip(&mut left_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(
        after["data"]["step_count"], 1,
        "a rejected frame must not count as a step"
    );

    let tools = rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {"session_id": left}}),
    )
    .await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo_message", "echo_with_length"]);

    let called = rpc(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {"session_id": left, "name": "echo_message", "arguments": {"message": "via-http"}}
        }),
    )
    .await;
    assert_eq!(called["result"]["data"], "via-http");
    let still = roundtrip(&mut left_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(
        still["data"]["step_count"], 1,
        "HTTP tools/call does not increment step_count"
    );

    let failed = roundtrip(
        &mut left_ws,
        &json!({"type": "step", "data": {"type": "call_tool", "tool_name": "nope", "arguments": {}}}).to_string(),
    )
    .await;
    assert_eq!(
        failed["data"]["observation"]["error"]["error_type"],
        "tool_not_found"
    );
    let counted = roundtrip(&mut left_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(counted["data"]["step_count"], 2);

    drop(left_ws);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let closed = rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 9, "method": "openenv/session/close", "params": {"session_id": left}}),
    )
    .await;
    assert!(
        closed.get("error").is_some(),
        "disconnect drops the session: {closed}"
    );

    let fresh = create_session(&client, &base).await;
    let mut fresh_ws = connect(&base, &fresh).await;
    let fresh_state = roundtrip(&mut fresh_ws, &json!({"type": "state"}).to_string()).await;
    assert_ne!(fresh_state["data"]["episode_id"], "left");
    assert_eq!(fresh_state["data"]["step_count"], 0);

    let right_again = roundtrip(&mut right_ws, &json!({"type": "state"}).to_string()).await;
    assert_eq!(right_again["data"]["episode_id"], "right");
    assert_eq!(right_again["data"]["step_count"], 0);
}

#[allow(clippy::unwrap_used)]
#[tokio::test]
async fn capacity_and_second_socket_are_rejected() {
    let base = spawn(1, Some(Duration::from_secs(30))).await;
    let client = reqwest::Client::new();
    let first = create_session(&client, &base).await;
    let second = rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "openenv/session/create", "params": {}}),
    )
    .await;
    assert_eq!(second["error"]["code"], -32000);
    assert_eq!(second["error"]["data"]["max_sessions"], 1);
    assert!(
        second["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Server at capacity")
    );

    let mut primary = connect(&base, &first).await;
    let url = format!("{}/ws?session_id={first}", base.replacen("http", "ws", 1));
    let (mut extra, _) = connect_async(url).await.unwrap();
    let rejection = next_text(&mut extra).await;
    assert_eq!(rejection["data"]["code"], "SESSION_ERROR");
    assert!(
        rejection["data"]["message"]
            .as_str()
            .unwrap()
            .contains("already has an attached WebSocket")
    );

    let state = roundtrip(&mut primary, &json!({"type": "state"}).to_string()).await;
    assert_eq!(state["type"], "state");
}

#[allow(clippy::unwrap_used)]
#[tokio::test]
async fn unattached_session_is_reaped() {
    let base = spawn(2, Some(Duration::from_millis(200))).await;
    let client = reqwest::Client::new();
    let stale = create_session(&client, &base).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _ = create_session(&client, &base).await;
    let gone = rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"session_id": stale}}),
    )
    .await;
    assert!(gone.get("error").is_some(), "{gone}");
}

#[allow(clippy::unwrap_used)]
async fn spawn(max_sessions: usize, idle: Option<Duration>) -> String {
    let state = AppState::new(ServerConfig {
        max_sessions,
        max_message_bytes: 1024 * 1024,
        idle_timeout: idle,
        kind: EnvKind::Echo,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = state.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

#[allow(clippy::unwrap_used)]
async fn create_session(client: &reqwest::Client, base: &str) -> String {
    let created = rpc(
        client,
        base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "openenv/session/create", "params": {}}),
    )
    .await;
    created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[allow(clippy::unwrap_used)]
async fn rpc(client: &reqwest::Client, base: &str, body: Value) -> Value {
    client
        .post(format!("{base}/mcp"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[allow(clippy::unwrap_used)]
async fn connect(
    base: &str,
    session: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("{}/ws?session_id={session}", base.replacen("http", "ws", 1));
    let (socket, _) = connect_async(url).await.unwrap();
    socket
}

#[allow(clippy::unwrap_used)]
async fn roundtrip(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    text: &str,
) -> Value {
    socket
        .send(Message::Text(text.to_string().into()))
        .await
        .unwrap();
    next_text(socket).await
}

#[allow(clippy::unwrap_used)]
async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match message {
        Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}
