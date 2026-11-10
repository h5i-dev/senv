#![allow(clippy::unwrap_used, clippy::panic)]
//! WebSocket coverage for `senv serve --env arith`.
//!
//! The unmodified `TextArenaEnv` client is exercised by `scripts/arith_compat.sh`.
//! These tests compute the expression value from the same pure generator the
//! server uses. They do not read it out of an observation or state frame.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use senv::arith::{self, generate};
use senv::{AppState, EnvKind, ServerConfig};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn arith_hides_the_value_scores_answers_and_isolates_sessions() {
    let (seed, value) = hidden_value_seed();
    let value_text = value.to_string();
    let base = spawn(3).await;

    let meta: Value = reqwest::get(format!("{base}/metadata"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta["name"], "arith_v0");
    let schema: Value = reqwest::get(format!("{base}/schema"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(schema["state"]["properties"]["env_id"]["const"], "Arith-v0");

    let mut left = connect(&base).await;
    let mut right = connect(&base).await;
    let reset = roundtrip(
        &mut left,
        &json!({"type": "reset", "data": {"episode_id": "safe", "seed": seed}}).to_string(),
    )
    .await;
    assert_eq!(reset["type"], "observation");
    assert_eq!(reset["data"]["reward"], 0.0);
    assert_eq!(reset["data"]["done"], false);
    let prompt = reset["data"]["observation"]["prompt"].as_str().unwrap();
    let expr = expression_line(prompt);
    assert_eq!(operator_count(expr), 3, "{expr}");
    assert!(
        !prompt.contains(&value_text),
        "prompt leaked {value_text}: {prompt}"
    );
    assert!(
        !reset.to_string().contains(&value_text),
        "reset frame leaked {value_text}: {reset}"
    );

    let state = roundtrip(&mut left, &json!({"type": "state"}).to_string()).await;
    assert_eq!(state["data"]["env_id"], "Arith-v0");
    assert_eq!(state["data"]["episode_id"], "safe");
    assert_eq!(state["data"]["step_count"], 0);
    assert_eq!(state["data"]["max_turns"], 8);
    let raw = &state["data"]["raw_state"];
    assert!(raw.get("value").is_none(), "{raw}");
    assert!(raw.get("answer").is_none(), "{raw}");
    assert!(raw.get("secret").is_none(), "{raw}");
    assert_eq!(raw["expression"], expr);
    assert!(
        !state.to_string().contains(&value_text),
        "state leaked {value_text}: {state}"
    );

    let calc = roundtrip(
        &mut left,
        &json!({"type": "step", "data": {"message": "calc 2 * 3"}}).to_string(),
    )
    .await;
    assert_eq!(calc["data"]["done"], false);
    assert_eq!(calc["data"]["reward"], 0.0);
    assert_eq!(
        calc["data"]["observation"]["info"]["arith.format_valid"],
        true
    );
    assert_eq!(calc["data"]["observation"]["info"]["arith.success"], false);
    let calc_text = joined(&calc["data"]["observation"]["messages"]);
    assert!(calc_text.contains("Result: 6"), "{calc_text}");
    assert!(
        !calc_text.contains(&value_text),
        "calc leaked {value_text}: {calc_text}"
    );

    let wrong_token = if value == 0 { 1 } else { 0 };
    let wrong = roundtrip(
        &mut left,
        &json!({"type": "step", "data": {"message": format!("answer {wrong_token}")}}).to_string(),
    )
    .await;
    assert_eq!(wrong["data"]["done"], true);
    assert_eq!(wrong["data"]["reward"], 0.0);
    let wrong_text = joined(&wrong["data"]["observation"]["messages"]);
    assert!(wrong_text.contains("Incorrect."), "{wrong_text}");
    assert!(
        !wrong_text.contains(&value_text),
        "wrong answer leaked {value_text}: {wrong_text}"
    );
    let wrong_state = roundtrip(&mut left, &json!({"type": "state"}).to_string()).await;
    assert!(wrong_state["data"]["raw_state"].get("value").is_none());
    assert_eq!(wrong_state["data"]["raw_state"]["last_reply"], "Incorrect.");
    assert!(
        !wrong_state.to_string().contains(&value_text),
        "wrong state leaked {value_text}"
    );

    let after = roundtrip(
        &mut left,
        &json!({"type": "step", "data": {"message": format!("answer {value}")}}).to_string(),
    )
    .await;
    assert_eq!(after["data"]["code"], "EXECUTION_ERROR");

    let mut solved = connect(&base).await;
    let _ = roundtrip(
        &mut solved,
        &json!({"type": "reset", "data": {"episode_id": "solve", "seed": seed}}).to_string(),
    )
    .await;
    let win = roundtrip(
        &mut solved,
        &json!({"type": "step", "data": {"message": format!("answer {value}")}}).to_string(),
    )
    .await;
    assert_eq!(win["data"]["done"], true);
    assert_eq!(win["data"]["reward"], 1.0);
    assert_eq!(win["data"]["observation"]["info"]["arith.success"], true);
    let replies: Vec<&str> = win["data"]["observation"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .collect();
    assert_eq!(replies.last().copied(), Some("Correct."));
    let win_state = roundtrip(&mut solved, &json!({"type": "state"}).to_string()).await;
    assert_eq!(win_state["data"]["step_count"], 1);
    assert_eq!(win_state["data"]["raw_state"]["last_reply"], "Correct.");
    assert!(win_state["data"]["raw_state"].get("value").is_none());

    let right_reset = roundtrip(
        &mut right,
        &json!({"type": "reset", "data": {"episode_id": "other", "seed": seed + 1}}).to_string(),
    )
    .await;
    assert_eq!(right_reset["data"]["done"], false);
    let right_expr = expression_line(
        right_reset["data"]["observation"]["prompt"]
            .as_str()
            .unwrap(),
    );
    assert_ne!(right_expr, expr);
    let right_state = roundtrip(&mut right, &json!({"type": "state"}).to_string()).await;
    assert_eq!(right_state["data"]["step_count"], 0);
    assert_eq!(right_state["data"]["episode_id"], "other");

    let mut limit = connect(&base).await;
    let _ = roundtrip(
        &mut limit,
        &json!({"type": "reset", "data": {"episode_id": "limit", "seed": seed}}).to_string(),
    )
    .await;
    let mut last = Value::Null;
    for _ in 0..8 {
        last = roundtrip(
            &mut limit,
            &json!({"type": "step", "data": {"message": "nope"}}).to_string(),
        )
        .await;
    }
    assert_eq!(last["data"]["done"], true);
    assert_eq!(last["data"]["reward"], 0.0);
    assert_eq!(
        last["data"]["observation"]["info"]["arith.format_valid"],
        false
    );
    let limited = joined(&last["data"]["observation"]["messages"]);
    assert!(limited.contains("Step limit reached."), "{limited}");
    assert!(!limited.contains(&value_text), "{limited}");
    let limit_state = roundtrip(&mut limit, &json!({"type": "state"}).to_string()).await;
    assert!(
        !limit_state.to_string().contains(&value_text),
        "{limit_state}"
    );

    let overflow = roundtrip(
        &mut right,
        &json!({"type": "step", "data": {"message": "calc 999999999999 * 999999999999"}})
            .to_string(),
    )
    .await;
    assert_eq!(overflow["data"]["done"], false);
    assert!(
        joined(&overflow["data"]["observation"]["messages"]).contains("Result: overflow"),
        "{overflow}"
    );

    let too_long = "x".repeat(4097);
    let rejected = roundtrip(
        &mut right,
        &json!({"type": "step", "data": {"message": too_long}}).to_string(),
    )
    .await;
    assert_eq!(rejected["data"]["code"], "VALIDATION_ERROR");
    let still = roundtrip(&mut right, &json!({"type": "state"}).to_string()).await;
    assert_eq!(still["data"]["step_count"], 1);
}

#[tokio::test]
async fn same_seed_is_the_same_expression() {
    let base = spawn(2).await;
    let mut a = connect(&base).await;
    let mut b = connect(&base).await;
    for (socket, episode) in [(&mut a, "a"), (&mut b, "b")] {
        let _ = roundtrip(
            socket,
            &json!({"type": "reset", "data": {"seed": 11, "episode_id": episode}}).to_string(),
        )
        .await;
    }
    let left = roundtrip(&mut a, &json!({"type": "state"}).to_string()).await;
    let right = roundtrip(&mut b, &json!({"type": "state"}).to_string()).await;
    assert_eq!(
        left["data"]["raw_state"]["expression"],
        right["data"]["raw_state"]["expression"]
    );
    assert_eq!(
        operator_count(left["data"]["raw_state"]["expression"].as_str().unwrap()),
        2
    );
}

fn hidden_value_seed() -> (i64, i64) {
    for seed in 0..5_000 {
        let problem = generate(seed, 3);
        let rendered = problem.value.to_string();
        if rendered.len() < 2 {
            continue;
        }
        let prompt = arith::prompt_text(&arith::expression(&problem));
        if !prompt.contains(&rendered) {
            return (seed, problem.value);
        }
    }
    panic!("no seed hid the value");
}

fn expression_line(prompt: &str) -> &str {
    prompt
        .lines()
        .find_map(|line| line.strip_prefix("Expression: "))
        .unwrap()
}

fn operator_count(expression: &str) -> usize {
    expression
        .chars()
        .filter(|ch| matches!(ch, '+' | '-' | '*'))
        .count()
}

async fn spawn(arith_ops: u8) -> String {
    let state = AppState::new(ServerConfig {
        max_sessions: 8,
        max_message_bytes: 1024 * 1024,
        idle_timeout: Some(Duration::from_secs(30)),
        kind: EnvKind::Arith,
        arith_ops,
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
