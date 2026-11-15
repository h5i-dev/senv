//! OpenEnv WebSocket frames that do not depend on the environment.
//!
//! Shapes follow `openenv/OpenEnv` at [`crate::OPENENV_GIT_REVISION`]: the
//! messages in `openenv/core/env_server/types.py` and observation packing in
//! `serialization.py`. Pydantic's `errors` array is not reproduced. A
//! validation failure is `{"type":"error","data":{"code":"VALIDATION_ERROR",...}}`.

use serde_json::{Map, Number, Value, json};

use crate::env::{Observation, Reward, WireError};

/// Longest accepted `episode_id`, in bytes.
pub const MAX_EPISODE_ID_BYTES: usize = 255;

/// One client message, with the environment-specific `step` payload left as JSON.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Reset {
        seed: Option<i64>,
        episode_id: Option<String>,
    },
    Step(Map<String, Value>),
    State,
    Close,
}

pub fn parse_incoming(text: &str) -> Result<Incoming, WireError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| WireError::new("INVALID_JSON", format!("Invalid JSON: {error}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| WireError::validation("Invalid message"))?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError::validation("Invalid message"))?;
    match ty {
        "reset" => {
            reject_unknown(obj, &["type", "data"])?;
            let data = object_field(obj, "data", true)?;
            Ok(Incoming::Reset {
                seed: parse_seed(&data)?,
                episode_id: parse_episode_id(&data)?,
            })
        }
        "step" => {
            reject_unknown(obj, &["type", "data"])?;
            Ok(Incoming::Step(object_field(obj, "data", false)?))
        }
        "state" => {
            reject_unknown(obj, &["type"])?;
            Ok(Incoming::State)
        }
        "close" => {
            reject_unknown(obj, &["type"])?;
            Ok(Incoming::Close)
        }
        other => Err(WireError::new(
            "UNKNOWN_TYPE",
            format!("Unknown message type: {other}"),
        )),
    }
}

/// Refuse keys outside `allowed`. Environments use this on their step payload.
pub fn reject_unknown(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), WireError> {
    for key in obj.keys() {
        if !allowed.iter().any(|name| name == key) {
            return Err(WireError::validation(format!(
                "Invalid message: unexpected field '{key}'"
            )));
        }
    }
    Ok(())
}

/// The string field `name` of a step payload, required.
pub fn string_field(obj: &Map<String, Value>, name: &str) -> Result<String, WireError> {
    match obj.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(WireError::validation(format!(
            "Invalid message: {name} must be a string"
        ))),
        None => Err(WireError::validation(format!(
            "Invalid message: {name} is required"
        ))),
    }
}

fn object_field(
    obj: &Map<String, Value>,
    name: &str,
    default_empty: bool,
) -> Result<Map<String, Value>, WireError> {
    match obj.get(name) {
        None if default_empty => Ok(Map::new()),
        None => Err(WireError::validation(format!(
            "Invalid message: missing {name}"
        ))),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(WireError::validation(format!(
            "Invalid message: {name} must be an object"
        ))),
    }
}

fn parse_seed(data: &Map<String, Value>) -> Result<Option<i64>, WireError> {
    match data.get("seed") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| WireError::validation("Invalid message: seed must be an integer")),
        Some(_) => Err(WireError::validation(
            "Invalid message: seed must be an integer",
        )),
    }
}

fn parse_episode_id(data: &Map<String, Value>) -> Result<Option<String>, WireError> {
    match data.get("episode_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.len() <= MAX_EPISODE_ID_BYTES => Ok(Some(id.clone())),
        Some(Value::String(_)) => Err(WireError::validation(format!(
            "Invalid message: episode_id longer than {MAX_EPISODE_ID_BYTES} bytes"
        ))),
        Some(_) => Err(WireError::validation(
            "Invalid message: episode_id must be a string",
        )),
    }
}

pub fn error_frame(error: &WireError) -> String {
    let mut data = Map::new();
    data.insert("message".to_string(), Value::String(error.message.clone()));
    data.insert("code".to_string(), Value::String(error.code.to_string()));
    for (key, value) in &error.extra {
        data.insert((*key).to_string(), value.clone());
    }
    frame("error", Value::Object(data))
}

pub fn observation_frame(observation: &Observation) -> String {
    let mut data = Map::new();
    data.insert("observation".to_string(), observation.observation.clone());
    data.insert("reward".to_string(), reward_value(observation.reward));
    data.insert("done".to_string(), Value::Bool(observation.done));
    if let Some(metadata) = &observation.metadata {
        data.insert("metadata".to_string(), metadata.clone());
    }
    frame("observation", Value::Object(data))
}

pub fn state_frame(state: Value) -> String {
    frame("state", state)
}

/// The JSON number for a reward, or `null`.
pub fn reward_value(reward: Reward) -> Value {
    reward
        .as_f64()
        .and_then(Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn frame(ty: &str, data: Value) -> String {
    json!({ "type": ty, "data": data }).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bad_json_and_unknown_type_are_different_errors() {
        assert_eq!(parse_incoming("{").unwrap_err().code, "INVALID_JSON");
        assert_eq!(
            parse_incoming(r#"{"type":"nope"}"#).unwrap_err().code,
            "UNKNOWN_TYPE"
        );
    }

    #[test]
    fn step_needs_a_data_object_and_reset_does_not() {
        assert_eq!(
            parse_incoming(r#"{"type":"step"}"#).unwrap_err().code,
            "VALIDATION_ERROR"
        );
        assert_eq!(
            parse_incoming(r#"{"type":"reset"}"#).unwrap(),
            Incoming::Reset {
                seed: None,
                episode_id: None
            }
        );
        assert_eq!(
            parse_incoming(r#"{"type":"reset","data":{"seed":1.5}}"#)
                .unwrap_err()
                .code,
            "VALIDATION_ERROR"
        );
        let long = "x".repeat(MAX_EPISODE_ID_BYTES + 1);
        let text = json!({"type": "reset", "data": {"episode_id": long}}).to_string();
        assert_eq!(parse_incoming(&text).unwrap_err().code, "VALIDATION_ERROR");
    }

    #[test]
    fn unknown_envelope_fields_are_refused() {
        assert_eq!(
            parse_incoming(r#"{"type":"state","extra":1}"#)
                .unwrap_err()
                .code,
            "VALIDATION_ERROR"
        );
    }

    #[test]
    fn rewards_render_as_numbers_or_null() {
        assert_eq!(reward_value(Reward::None), Value::Null);
        assert_eq!(reward_value(Reward::Milli(250)), json!(0.25));
        assert_eq!(reward_value(Reward::Float(f64::NAN)), Value::Null);
        let frame: Value = serde_json::from_str(&observation_frame(&Observation::new(
            json!({"x": 1}),
            Reward::Milli(0),
            false,
        )))
        .unwrap();
        assert_eq!(frame["type"], "observation");
        assert_eq!(frame["data"]["reward"], 0.0);
        assert!(frame["data"].get("metadata").is_none());
    }
}
