//! OpenEnv JSON for Echo.
//!
//! Shapes follow `meta-pytorch/OpenEnv` at the pinned revision: WebSocket
//! messages in `openenv/core/env_server/types.py` and observation packing in
//! `serialization.py`. Pydantic's `errors` array is not reproduced. A
//! validation failure is `{"type":"error","data":{"code":"VALIDATION_ERROR",...}}`.

use serde_json::{Map, Number, Value};

use crate::kernel::{self, Body, Outcome, StepAction, ToolCall, ToolErrorType};

pub const MAX_EPISODE_ID_BYTES: usize = 255;

#[derive(Debug)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
    pub extra: Vec<(&'static str, Value)>,
}

impl WireError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            extra: Vec::new(),
        }
    }

    pub fn with_extra(mut self, extra: Vec<(&'static str, Value)>) -> Self {
        self.extra = extra;
        self
    }
}

#[derive(Debug)]
pub enum Incoming {
    Reset {
        seed: Option<i64>,
        episode_id: Option<String>,
    },
    Step(StepAction),
    State,
    Close,
}

pub fn parse_incoming(text: &str) -> Result<Incoming, WireError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| WireError::new("INVALID_JSON", format!("Invalid JSON: {error}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| WireError::new("VALIDATION_ERROR", "Invalid message"))?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError::new("VALIDATION_ERROR", "Invalid message"))?;
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
            let data = object_field(obj, "data", false)?;
            Ok(Incoming::Step(parse_action(&data)?))
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

fn reject_unknown(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), WireError> {
    for key in obj.keys() {
        if !allowed.iter().any(|name| name == key) {
            return Err(WireError::new(
                "VALIDATION_ERROR",
                format!("Invalid message: unexpected field '{key}'"),
            ));
        }
    }
    Ok(())
}

fn object_field(
    obj: &Map<String, Value>,
    name: &str,
    default_empty: bool,
) -> Result<Map<String, Value>, WireError> {
    match obj.get(name) {
        None if default_empty => Ok(Map::new()),
        None => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: missing {name}"),
        )),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: {name} must be an object"),
        )),
    }
}

fn parse_seed(data: &Map<String, Value>) -> Result<Option<i64>, WireError> {
    match data.get("seed") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_i64().map(Some).ok_or_else(|| {
            WireError::new(
                "VALIDATION_ERROR",
                "Invalid message: seed must be an integer",
            )
        }),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: seed must be an integer",
        )),
    }
}

fn parse_episode_id(data: &Map<String, Value>) -> Result<Option<String>, WireError> {
    match data.get("episode_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.len() <= MAX_EPISODE_ID_BYTES => Ok(Some(id.clone())),
        Some(Value::String(_)) => Err(WireError::new(
            "VALIDATION_ERROR",
            format!("Invalid message: episode_id longer than {MAX_EPISODE_ID_BYTES} bytes"),
        )),
        Some(_) => Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: episode_id must be a string",
        )),
    }
}

fn parse_action(data: &Map<String, Value>) -> Result<StepAction, WireError> {
    let ty = data.get("type").and_then(Value::as_str).ok_or_else(|| {
        WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: action type is required",
        )
    })?;
    match ty {
        "list_tools" => {
            reject_unknown(data, &["type", "metadata"])?;
            require_object_metadata(data)?;
            Ok(StepAction::ListTools)
        }
        "call_tool" => {
            reject_unknown(data, &["type", "tool_name", "arguments", "metadata"])?;
            require_object_metadata(data)?;
            let name = data
                .get("tool_name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WireError::new(
                        "VALIDATION_ERROR",
                        "Invalid message: tool_name must be a string",
                    )
                })?;
            let arguments = match data.get("arguments") {
                None => Map::new(),
                Some(Value::Object(map)) => map.clone(),
                Some(_) => {
                    return Err(WireError::new(
                        "VALIDATION_ERROR",
                        "Invalid message: arguments must be an object",
                    ));
                }
            };
            Ok(StepAction::Call(tool_call(name, &arguments)))
        }
        _ => Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: action must be list_tools or call_tool",
        )),
    }
}

/// A well-typed `call_tool` whose tool then fails is still a step. The failure
/// is a tool observation, not a validation error, matching `MCPEnvironment`.
pub fn tool_call(name: &str, args: &Map<String, Value>) -> ToolCall {
    match name {
        "echo_message" => match message_arg(args, "echo_message") {
            Ok(message) => ToolCall::EchoMessage { message },
            Err(message) => ToolCall::Failed {
                name: name.to_string(),
                error_type: ToolErrorType::Execution,
                message,
            },
        },
        "echo_with_length" => match message_arg(args, "echo_with_length") {
            Ok(Value::String(message)) => ToolCall::EchoWithLength { message },
            Ok(_) => ToolCall::Failed {
                name: name.to_string(),
                error_type: ToolErrorType::InvalidArgs,
                message: "echo_with_length() argument 'message' must be a string".into(),
            },
            Err(message) => ToolCall::Failed {
                name: name.to_string(),
                error_type: ToolErrorType::Execution,
                message,
            },
        },
        other => ToolCall::Failed {
            name: other.to_string(),
            error_type: ToolErrorType::NotFound,
            message: format!("Unknown tool: {other}"),
        },
    }
}

fn message_arg(args: &Map<String, Value>, func: &str) -> Result<Value, String> {
    if let Some(key) = args.keys().find(|key| key.as_str() != "message") {
        return Err(format!(
            "{func}() got an unexpected keyword argument '{key}'"
        ));
    }
    match args.get("message") {
        None => Err(format!(
            "{func}() missing 1 required positional argument: 'message'"
        )),
        Some(value) => Ok(value.clone()),
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

pub fn observation_frame(outcome: &Outcome) -> String {
    let (observation, metadata) = match &outcome.body {
        Body::Ready => {
            let metadata = serde_json::json!({
                "status": "ready",
                "message": "Echo environment ready!"
            });
            (serde_json::json!({ "metadata": metadata }), Some(metadata))
        }
        Body::Tools => (
            serde_json::json!({
                "metadata": {},
                "tools": tool_records(false),
            }),
            None,
        ),
        Body::Called { name, output } => {
            let text = match output {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (
                serde_json::json!({
                    "metadata": {},
                    "tool_name": name,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "structured_content": { "result": output },
                        "meta": null,
                        "data": output,
                        "is_error": false
                    },
                    "error": null
                }),
                None,
            )
        }
        Body::Failed {
            name,
            error_type,
            message,
        } => (
            serde_json::json!({
                "metadata": {},
                "tool_name": name,
                "result": null,
                "error": {
                    "error_type": error_type.as_wire(),
                    "message": message
                }
            }),
            None,
        ),
    };
    let mut data = Map::new();
    data.insert("observation".to_string(), observation);
    data.insert("reward".to_string(), reward_value(outcome.reward_milli));
    data.insert("done".to_string(), Value::Bool(outcome.done));
    if let Some(metadata) = metadata {
        data.insert("metadata".to_string(), metadata);
    }
    frame("observation", Value::Object(data))
}

pub fn state_frame(episode_id: &str, step_count: u64) -> String {
    frame(
        "state",
        serde_json::json!({
            "episode_id": episode_id,
            "step_count": step_count
        }),
    )
}

fn frame(ty: &str, data: Value) -> String {
    serde_json::json!({ "type": ty, "data": data }).to_string()
}

fn reward_value(milli: Option<i64>) -> Value {
    match milli {
        None => Value::Null,
        Some(milli) => {
            let number = (milli as f64) / 1000.0;
            match Number::from_f64(number) {
                Some(number) => Value::Number(number),
                None => Value::Null,
            }
        }
    }
}

fn tool_records(http_schema_key: bool) -> Vec<Value> {
    let schema_key = if http_schema_key {
        "inputSchema"
    } else {
        "input_schema"
    };
    kernel::tool_specs()
        .iter()
        .map(|(name, description)| {
            serde_json::json!({
                "name": name,
                "description": description,
                schema_key: input_schema(name),
            })
        })
        .collect()
}

fn require_object_metadata(data: &Map<String, Value>) -> Result<(), WireError> {
    if let Some(metadata) = data.get("metadata")
        && !metadata.is_object()
    {
        return Err(WireError::new(
            "VALIDATION_ERROR",
            "Invalid message: metadata must be an object",
        ));
    }
    Ok(())
}

fn input_schema(_name: &str) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
}

pub fn http_tools() -> Vec<Value> {
    tool_records(true)
}

pub fn schema_document() -> Value {
    serde_json::json!({
        "action": {
            "type": "object",
            "properties": {
                "type": { "enum": ["list_tools", "call_tool"] },
                "tool_name": { "type": "string" },
                "arguments": { "type": "object" }
            },
            "required": ["type"]
        },
        "observation": {
            "type": "object",
            "description": "CallToolObservation or ListToolsObservation packed by serialize_observation"
        },
        "reset_observation": {
            "type": "object",
            "properties": {
                "metadata": {
                    "type": "object",
                    "properties": {
                        "status": { "const": "ready" },
                        "message": { "const": "Echo environment ready!" }
                    }
                }
            }
        },
        "state": {
            "type": "object",
            "properties": {
                "episode_id": { "type": "string" },
                "step_count": { "type": "integer", "minimum": 0 }
            },
            "required": ["step_count"]
        },
        "reset": {
            "type": "object",
            "properties": {
                "seed": { "type": "integer" },
                "episode_id": { "type": "string", "maxLength": MAX_EPISODE_ID_BYTES }
            }
        }
    })
}

pub fn metadata_document() -> Value {
    serde_json::json!({
        "name": "echo_env",
        "description": "Echo environment. reset, step and state follow the OpenEnv WebSocket protocol. Tools: echo_message, echo_with_length.",
        "version": env!("CARGO_PKG_VERSION"),
        "author": "h5i-dev",
        "documentation_url": "https://github.com/h5i-dev/senv"
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_message_type_without_confusing_it_with_bad_json() {
        let bad = parse_incoming("{").unwrap_err();
        assert_eq!(bad.code, "INVALID_JSON");
        let unknown = parse_incoming(r#"{"type":"nope"}"#).unwrap_err();
        assert_eq!(unknown.code, "UNKNOWN_TYPE");
    }

    #[test]
    fn step_missing_data_does_not_become_a_tool_call() {
        let error = parse_incoming(r#"{"type":"step"}"#).unwrap_err();
        assert_eq!(error.code, "VALIDATION_ERROR");
    }

    #[test]
    fn call_tool_parses_and_unknown_tool_is_a_step() {
        let incoming = parse_incoming(
            r#"{"type":"step","data":{"type":"call_tool","tool_name":"echo_message","arguments":{"message":"Hi"}}}"#,
        )
        .unwrap();
        match incoming {
            Incoming::Step(StepAction::Call(ToolCall::EchoMessage { message })) => {
                assert_eq!(message, Value::String("Hi".into()));
            }
            other => panic!("unexpected {other:?}"),
        }
        let missing = parse_incoming(
            r#"{"type":"step","data":{"type":"call_tool","tool_name":"nope","arguments":{}}}"#,
        )
        .unwrap();
        match missing {
            Incoming::Step(StepAction::Call(ToolCall::Failed { error_type, .. })) => {
                assert_eq!(error_type, ToolErrorType::NotFound);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ready_observation_carries_zero_reward_and_the_upstream_message() {
        let outcome = Outcome {
            body: Body::Ready,
            reward_milli: Some(0),
            done: false,
        };
        let parsed: Value = serde_json::from_str(&observation_frame(&outcome)).unwrap();
        assert_eq!(parsed["type"], "observation");
        assert_eq!(parsed["data"]["reward"], 0.0);
        assert_eq!(parsed["data"]["done"], false);
        assert_eq!(
            parsed["data"]["metadata"]["message"],
            "Echo environment ready!"
        );
        assert_eq!(
            parsed["data"]["observation"]["metadata"]["message"],
            "Echo environment ready!"
        );
    }
}
