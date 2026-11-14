#![allow(clippy::unwrap_used, clippy::panic)]
//! senv's server over real sockets, with a two-line kernel instead of an example.
//!
//! The examples under `examples/` have their own protocol tests. This file
//! checks the parts that belong to senv: routing, the envelope, rewards on the
//! wire, refusals, MCP defaults, capacity, and per-session isolation.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use senv::{
    Environment, Kernel, Metadata, Observation, Phase, Reward, Server, ServerConfig, TenantId,
    WireError,
};
use serde_json::{Map, Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Counts pulls; arm 1 pays 1. Done after `limit` pulls.
#[derive(Clone, Default)]
struct Snap {
    pulls: u64,
    paid: i64,
    done: bool,
    episode: String,
}

enum Cmd {
    Reset(String),
    Pull(u8),
    State,
}

#[derive(Debug)]
enum Error {
    Done,
}

struct Bandit;

impl Kernel for Bandit {
    type Principal = ();
    type Snapshot = Snap;
    type Command = Cmd;
    type WriteSet = Snap;
    type Reply = ();
    type Error = Error;

    fn tenant(_: &()) -> TenantId {
        TenantId(1)
    }

    fn transition(_: &(), s: &Snap, c: &Cmd) -> Result<(Snap, ()), Error> {
        match c {
            Cmd::Reset(episode) => Ok((
                Snap {
                    episode: episode.clone(),
                    ..Snap::default()
                },
                (),
            )),
            Cmd::Pull(_) if s.done => Err(Error::Done),
            Cmd::Pull(arm) => {
                let pulls = s.pulls + 1;
                Ok((
                    Snap {
                        pulls,
                        paid: if *arm == 1 { 1000 } else { 0 },
                        done: pulls >= 2,
                        episode: s.episode.clone(),
                    },
                    (),
                ))
            }
            Cmd::State => Ok((s.clone(), ())),
        }
    }

    fn apply(_: &Snap, w: &Snap) -> Snap {
        w.clone()
    }
}

struct BanditEnv;

impl Environment for BanditEnv {
    type Kernel = Bandit;

    fn metadata(&self) -> Metadata {
        Metadata::new("bandit", "Two arms; arm 1 pays.")
    }

    fn principal(&self) {}

    fn reset(&self, _seed: Option<i64>, episode_id: String) -> Cmd {
        Cmd::Reset(episode_id)
    }

    fn step(&self, action: &Map<String, Value>) -> Result<Cmd, WireError> {
        match action.get("arm").and_then(Value::as_u64) {
            Some(arm @ (0 | 1)) => Ok(Cmd::Pull(arm as u8)),
            _ => Err(WireError::validation("arm must be 0 or 1")),
        }
    }

    fn state(&self) -> Cmd {
        Cmd::State
    }

    fn observe(&self, phase: Phase, s: &Snap, _: &()) -> Observation {
        let reward = match phase {
            Phase::Reset => Reward::Milli(0),
            Phase::Step => Reward::Milli(s.paid),
        };
        Observation::new(json!({ "pulls": s.pulls }), reward, s.done)
    }

    fn render_state(&self, s: &Snap, _: &()) -> Value {
        json!({ "episode_id": s.episode, "step_count": s.pulls })
    }

    fn refuse(&self, _: &Error) -> WireError {
        WireError::execution("episode is done")
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn(max_sessions: usize) -> String {
    let server = Server::new(
        BanditEnv,
        ServerConfig {
            max_sessions,
            max_message_bytes: 1024 * 1024,
            idle_timeout: Some(Duration::from_secs(30)),
        },
    );
    let addr = server.spawn("127.0.0.1:0").await.unwrap();
    format!("http://{addr}")
}

async fn connect(base: &str) -> Socket {
    let (socket, _) = connect_async(format!("{}/ws", base.replacen("http", "ws", 1)))
        .await
        .unwrap();
    socket
}

async fn roundtrip(socket: &mut Socket, message: Value) -> Value {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match reply {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn episodes_rewards_refusals_and_isolation() {
    let base = spawn(4).await;
    let meta: Value = reqwest::get(format!("{base}/metadata"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta["name"], "bandit");

    let mut left = connect(&base).await;
    let mut right = connect(&base).await;

    let reset = roundtrip(
        &mut left,
        json!({"type": "reset", "data": {"episode_id": "L"}}),
    )
    .await;
    assert_eq!(reset["type"], "observation");
    assert_eq!(reset["data"]["reward"], 0.0);
    assert_eq!(reset["data"]["done"], false);

    let pull = roundtrip(&mut left, json!({"type": "step", "data": {"arm": 1}})).await;
    assert_eq!(pull["data"]["reward"], 1.0);
    assert_eq!(pull["data"]["observation"]["pulls"], 1);

    let bad = roundtrip(&mut left, json!({"type": "step", "data": {"arm": 7}})).await;
    assert_eq!(bad["type"], "error");
    assert_eq!(bad["data"]["code"], "VALIDATION_ERROR");

    let last = roundtrip(&mut left, json!({"type": "step", "data": {"arm": 0}})).await;
    assert_eq!(last["data"]["reward"], 0.0);
    assert_eq!(last["data"]["done"], true);

    let refused = roundtrip(&mut left, json!({"type": "step", "data": {"arm": 1}})).await;
    assert_eq!(refused["data"]["code"], "EXECUTION_ERROR");

    let state = roundtrip(&mut left, json!({"type": "state"})).await;
    assert_eq!(state["type"], "state");
    assert_eq!(state["data"]["episode_id"], "L");
    assert_eq!(
        state["data"]["step_count"], 2,
        "the refused pull committed nothing"
    );

    roundtrip(
        &mut right,
        json!({"type": "reset", "data": {"episode_id": "R"}}),
    )
    .await;
    let other = roundtrip(&mut right, json!({"type": "state"})).await;
    assert_eq!(other["data"]["episode_id"], "R");
    assert_eq!(other["data"]["step_count"], 0);

    let unknown = roundtrip(&mut right, json!({"type": "dance"})).await;
    assert_eq!(unknown["data"]["code"], "UNKNOWN_TYPE");
}

#[tokio::test]
async fn mcp_defaults_and_capacity() {
    let base = spawn(1).await;
    let client = reqwest::Client::new();
    let rpc = |body: Value| {
        let client = client.clone();
        let url = format!("{base}/mcp");
        async move {
            client
                .post(url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let created = rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "openenv/session/create"})).await;
    let session = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let tools = rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"session_id": session}})).await;
    assert_eq!(tools["result"]["tools"], json!([]));

    let call = rpc(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"session_id": session, "name": "x"}})).await;
    assert_eq!(call["error"]["code"], -32602);

    let full = rpc(json!({"jsonrpc": "2.0", "id": 4, "method": "openenv/session/create"})).await;
    assert_eq!(full["error"]["code"], -32000);
    assert_eq!(full["error"]["data"]["max_sessions"], 1);

    let closed = rpc(json!({"jsonrpc": "2.0", "id": 5, "method": "openenv/session/close", "params": {"session_id": session}})).await;
    assert_eq!(closed["result"]["closed"], true);
}
