#![allow(clippy::unwrap_used, clippy::panic)]
//! WebSocket coverage for `senv serve --env wordle`.
//!
//! The unmodified `TextArenaEnv` client is exercised by `scripts/wordle_compat.sh`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use senv::{AppState, EnvKind, ServerConfig};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn wordle_feedback_win_invalid_and_isolation() {
    let base = spawn().await;
    let mut left = connect(&base).await;
    let mut right = connect(&base).await;

    let reset = roundtrip(
        &mut left,
        &json!({"type": "reset", "data": {"episode_id": "wl", "seed": 7}}).to_string(),
    )
    .await;
    assert_eq!(reset["type"], "observation");
    assert_eq!(reset["data"]["reward"], 0.0);
    assert_eq!(reset["data"]["done"], false);
    let prompt = reset["data"]["observation"]["prompt"].as_str().unwrap();
    assert!(prompt.contains("You are Playing Wordle."));
    assert!(prompt.contains("You have 6 attempts"));

    let state = roundtrip(&mut left, &json!({"type": "state"}).to_string()).await;
    assert_eq!(state["data"]["episode_id"], "wl");
    assert_eq!(state["data"]["step_count"], 0);
    assert_eq!(state["data"]["env_id"], "Wordle-v0");
    assert_eq!(state["data"]["turn"], 0);
    let secret = state["data"]["raw_state"]["game_state"]["secret_word"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(secret.len(), 5);

    let other = if secret == "crane" { "apple" } else { "crane" };
    let step = roundtrip(
        &mut left,
        &json!({"type": "step", "data": {"message": format!("[{other}]")}}).to_string(),
    )
    .await;
    assert_eq!(step["data"]["done"], false);
    assert_eq!(step["data"]["reward"], 0.0);
    let text = joined(&step["data"]["observation"]["messages"]);
    assert!(
        text.contains(&format!("You submitted [{other}].\nFeedback:\n")),
        "{text}"
    );
    assert!(text.contains("You have 5 guesses left."), "{text}");
    let gyx = text.lines().any(|line| {
        let compact = line.replace(' ', "");
        !compact.is_empty() && compact.chars().all(|ch| matches!(ch, 'G' | 'Y' | 'X'))
    });
    assert!(gyx, "{text}");
    let signals = &step["data"]["observation"]["info"]["reward_signals"];
    assert!(signals["wordle.greens"].is_number());
    assert_eq!(signals["wordle.repetitions"], 1.0);
    assert_eq!(signals["wordle.correct"], 0.0);

    let win = roundtrip(
        &mut left,
        &json!({"type": "step", "data": {"message": format!("[{secret}]")}}).to_string(),
    )
    .await;
    assert_eq!(win["data"]["done"], true);
    assert_eq!(win["data"]["reward"], 1.0);
    let won = joined(&win["data"]["observation"]["messages"]);
    assert!(
        won.contains(&format!(
            "[{secret}]\n[GAME] Congratulations! You guessed the word correctly!"
        )),
        "{won}"
    );
    let after = roundtrip(&mut left, &json!({"type": "state"}).to_string()).await;
    assert_eq!(after["data"]["step_count"], 2);
    assert_eq!(after["data"]["turn"], 2);
    assert_eq!(after["data"]["raw_state"]["won"], true);

    let right_state = roundtrip(&mut right, &json!({"type": "state"}).to_string()).await;
    assert_eq!(right_state["data"]["step_count"], 0);
    let right_reset = roundtrip(
        &mut right,
        &json!({"type": "reset", "data": {"episode_id": "wr", "seed": 8}}).to_string(),
    )
    .await;
    assert_eq!(right_reset["data"]["reward"], 0.0);
    let right_secret = roundtrip(&mut right, &json!({"type": "state"}).to_string()).await;
    assert_eq!(right_secret["data"]["episode_id"], "wr");
    assert_ne!(
        right_secret["data"]["raw_state"]["game_state"]["secret_word"],
        secret
    );

    let mut invalid = connect(&base).await;
    let _ = roundtrip(
        &mut invalid,
        &json!({"type": "reset", "data": {"seed": 7, "episode_id": "bad"}}).to_string(),
    )
    .await;
    let first = roundtrip(
        &mut invalid,
        &json!({"type": "step", "data": {"message": "no brackets"}}).to_string(),
    )
    .await;
    assert_eq!(first["data"]["done"], false);
    assert!(joined(&first["data"]["observation"]["messages"]).contains("invalid move"));
    let second = roundtrip(
        &mut invalid,
        &json!({"type": "step", "data": {"message": "still bad"}}).to_string(),
    )
    .await;
    assert_eq!(second["data"]["done"], true);
    assert_eq!(second["data"]["reward"], 0.0);
    let ended = roundtrip(&mut invalid, &json!({"type": "state"}).to_string()).await;
    assert_eq!(ended["data"]["step_count"], 2);
    assert_eq!(ended["data"]["turn"], 0);

    let bad_json = roundtrip(&mut invalid, "{").await;
    assert_eq!(bad_json["data"]["code"], "INVALID_JSON");
}

#[tokio::test]
async fn same_seed_picks_the_same_secret() {
    let base = spawn().await;
    let mut a = connect(&base).await;
    let mut b = connect(&base).await;
    for (socket, episode) in [(&mut a, "a"), (&mut b, "b")] {
        let _ = roundtrip(
            socket,
            &json!({"type": "reset", "data": {"seed": 7, "episode_id": episode}}).to_string(),
        )
        .await;
    }
    let left = roundtrip(&mut a, &json!({"type": "state"}).to_string()).await;
    let right = roundtrip(&mut b, &json!({"type": "state"}).to_string()).await;
    assert_eq!(
        left["data"]["raw_state"]["game_state"]["secret_word"],
        right["data"]["raw_state"]["game_state"]["secret_word"]
    );
}

async fn spawn() -> String {
    let state = AppState::new(ServerConfig {
        max_sessions: 8,
        max_message_bytes: 1024 * 1024,
        idle_timeout: Some(Duration::from_secs(30)),
        kind: EnvKind::Wordle,
        arith_ops: 3,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = state.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

async fn connect(
    base: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("{}/ws", base.replacen("http", "ws", 1));
    let (socket, _) = connect_async(url).await.unwrap();
    socket
}

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
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match message {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn joined(messages: &Value) -> String {
    messages
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .collect()
}
