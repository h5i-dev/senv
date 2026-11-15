//! Echo as a senv environment: the OpenEnv Echo MCP environment.
//!
//! The kernel (`echo-kernel`) decides step counts and rewards. This crate is
//! the trusted shell. It binds the kernel to h5i-app, parses OpenEnv
//! `CallToolAction` / `ListToolsAction` payloads, and renders observations in
//! the shapes the unmodified `EchoEnv` client expects, following
//! `envs/echo_env` and `openenv/core` at [`senv::OPENENV_GIT_REVISION`].

use echo_kernel as k;
use senv::wire::reject_unknown;
use senv::{
    Environment, Kernel, Metadata, Observation, Phase, Reward, TenantId, Tool, ToolError, WireError,
};
use serde_json::{Map, Value, json};

/// The h5i-app binding of the Echo kernel.
pub struct EchoKernel;

impl Kernel for EchoKernel {
    type Principal = k::Principal;
    type Snapshot = k::Snapshot;
    type Command = k::Command;
    type WriteSet = k::Snapshot;
    type Reply = k::Reply;
    type Error = k::EchoError;

    fn tenant(actor: &k::Principal) -> TenantId {
        TenantId(actor.tenant)
    }

    fn transition(
        actor: &k::Principal,
        snap: &k::Snapshot,
        cmd: &k::Command,
    ) -> Result<(k::Snapshot, k::Reply), k::EchoError> {
        k::transition(actor, snap, cmd)
    }

    fn apply(snap: &k::Snapshot, ws: &k::Snapshot) -> k::Snapshot {
        k::apply(snap, ws)
    }

    fn read_only(cmd: &k::Command) -> bool {
        matches!(cmd, k::Command::State)
    }
}

/// The Echo environment. It has no configuration.
#[derive(Clone, Copy, Debug, Default)]
pub struct Echo;

impl Environment for Echo {
    type Kernel = EchoKernel;

    fn metadata(&self) -> Metadata {
        Metadata::new(
            "echo_env",
            "Echo environment. reset, step and state follow the OpenEnv WebSocket protocol. Tools: echo_message, echo_with_length.",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .author("h5i-dev")
        .documentation_url("https://github.com/h5i-dev/senv")
    }

    fn schema(&self) -> Value {
        schema_document()
    }

    fn principal(&self) -> k::Principal {
        k::Principal {
            tenant: k::SESSION_TENANT,
        }
    }

    fn initial(&self, episode_id: &str) -> k::Snapshot {
        k::Snapshot {
            episode_id: episode_id.to_string(),
            ..k::Snapshot::default()
        }
    }

    fn reset(&self, seed: Option<i64>, episode_id: String) -> k::Command {
        k::Command::Reset { seed, episode_id }
    }

    fn step(&self, action: &Map<String, Value>) -> Result<k::Command, WireError> {
        parse_action(action).map(k::Command::Step)
    }

    fn state(&self) -> k::Command {
        k::Command::State
    }

    fn observe(&self, _phase: Phase, _snapshot: &k::Snapshot, reply: &k::Reply) -> Observation {
        match reply {
            k::Reply::Outcome(outcome) => observation(outcome),
            // `transition` answers Reset and Step with an outcome. Render the
            // impossible case as an empty observation rather than panic.
            k::Reply::State { .. } => {
                Observation::new(json!({ "metadata": {} }), Reward::None, false)
            }
        }
    }

    fn render_state(&self, snapshot: &k::Snapshot, _reply: &k::Reply) -> Value {
        json!({
            "episode_id": snapshot.episode_id,
            "step_count": snapshot.step_count,
        })
    }

    fn refuse(&self, error: &k::EchoError) -> WireError {
        match error {
            k::EchoError::StepCountOverflow => WireError::execution("step count overflowed"),
        }
    }

    fn tools(&self) -> Vec<Tool> {
        k::tool_specs()
            .iter()
            .map(|(name, description)| Tool::new(*name, *description, input_schema()))
            .collect()
    }

    fn call_tool(&self, name: &str, arguments: &Map<String, Value>) -> Option<k::Command> {
        Some(k::Command::Invoke(k::StepAction::Call(tool_call(
            name, arguments,
        ))))
    }

    fn tool_result(&self, _snapshot: &k::Snapshot, reply: &k::Reply) -> Result<Value, ToolError> {
        match reply {
            k::Reply::Outcome(k::Outcome {
                body: k::Body::Called { output, .. },
                ..
            }) => Ok(output.clone()),
            k::Reply::Outcome(k::Outcome {
                body:
                    k::Body::Failed {
                        error_type,
                        message,
                        ..
                    },
                ..
            }) => Err(match error_type {
                k::ToolErrorType::NotFound | k::ToolErrorType::InvalidArgs => {
                    ToolError::InvalidArgs(message.clone())
                }
                k::ToolErrorType::Execution => ToolError::Execution(message.clone()),
            }),
            _ => Err(ToolError::Execution(
                "tool call did not return a tool result".into(),
            )),
        }
    }
}

fn parse_action(data: &Map<String, Value>) -> Result<k::StepAction, WireError> {
    let ty = data
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError::validation("Invalid message: action type is required"))?;
    match ty {
        "list_tools" => {
            reject_unknown(data, &["type", "metadata"])?;
            require_object_metadata(data)?;
            Ok(k::StepAction::ListTools)
        }
        "call_tool" => {
            reject_unknown(data, &["type", "tool_name", "arguments", "metadata"])?;
            require_object_metadata(data)?;
            let name = data
                .get("tool_name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WireError::validation("Invalid message: tool_name must be a string")
                })?;
            let arguments = match data.get("arguments") {
                None => Map::new(),
                Some(Value::Object(map)) => map.clone(),
                Some(_) => {
                    return Err(WireError::validation(
                        "Invalid message: arguments must be an object",
                    ));
                }
            };
            Ok(k::StepAction::Call(tool_call(name, &arguments)))
        }
        _ => Err(WireError::validation(
            "Invalid message: action must be list_tools or call_tool",
        )),
    }
}

/// A well-typed `call_tool` whose tool then fails is still a step. The failure
/// is a tool observation, not a validation error, matching `MCPEnvironment`.
pub fn tool_call(name: &str, args: &Map<String, Value>) -> k::ToolCall {
    match name {
        "echo_message" => match message_arg(args, "echo_message") {
            Ok(message) => k::ToolCall::EchoMessage { message },
            Err(message) => k::ToolCall::Failed {
                name: name.to_string(),
                error_type: k::ToolErrorType::Execution,
                message,
            },
        },
        "echo_with_length" => match message_arg(args, "echo_with_length") {
            Ok(Value::String(message)) => k::ToolCall::EchoWithLength { message },
            Ok(_) => k::ToolCall::Failed {
                name: name.to_string(),
                error_type: k::ToolErrorType::InvalidArgs,
                message: "echo_with_length() argument 'message' must be a string".into(),
            },
            Err(message) => k::ToolCall::Failed {
                name: name.to_string(),
                error_type: k::ToolErrorType::Execution,
                message,
            },
        },
        other => k::ToolCall::Failed {
            name: other.to_string(),
            error_type: k::ToolErrorType::NotFound,
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

fn require_object_metadata(data: &Map<String, Value>) -> Result<(), WireError> {
    match data.get("metadata") {
        Some(metadata) if !metadata.is_object() => Err(WireError::validation(
            "Invalid message: metadata must be an object",
        )),
        _ => Ok(()),
    }
}

fn observation(outcome: &k::Outcome) -> Observation {
    let reward = match outcome.reward_milli {
        Some(milli) => Reward::Milli(milli),
        None => Reward::None,
    };
    match &outcome.body {
        k::Body::Ready => {
            let metadata = json!({
                "status": "ready",
                "message": "Echo environment ready!"
            });
            Observation::new(json!({ "metadata": metadata }), reward, outcome.done)
                .with_metadata(metadata)
        }
        k::Body::Tools => Observation::new(
            json!({ "metadata": {}, "tools": websocket_tools() }),
            reward,
            outcome.done,
        ),
        k::Body::Called { name, output } => {
            let text = match output {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            Observation::new(
                json!({
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
                reward,
                outcome.done,
            )
        }
        k::Body::Failed {
            name,
            error_type,
            message,
        } => Observation::new(
            json!({
                "metadata": {},
                "tool_name": name,
                "result": null,
                "error": {
                    "error_type": error_type.as_wire(),
                    "message": message
                }
            }),
            reward,
            outcome.done,
        ),
    }
}

/// `ListToolsObservation` uses snake_case `input_schema`; HTTP `tools/list`
/// uses camelCase `inputSchema`, which senv's server writes.
fn websocket_tools() -> Vec<Value> {
    k::tool_specs()
        .iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": description,
                "input_schema": input_schema(),
            })
        })
        .collect()
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
}

fn schema_document() -> Value {
    json!({
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
                "episode_id": { "type": "string", "maxLength": senv::wire::MAX_EPISODE_ID_BYTES }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use senv::Session;

    fn session() -> Session<Echo> {
        Session::open(Arc::new(Echo), "ep")
    }

    fn action(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn reset_is_ready_with_zero_reward() {
        let observation = session().reset(Some(1), Some("ep".into())).unwrap();
        assert_eq!(observation.reward, Reward::Milli(0));
        assert!(!observation.done);
        assert_eq!(
            observation.observation["metadata"]["message"],
            "Echo environment ready!"
        );
    }

    #[test]
    fn steps_count_and_tool_calls_do_not() {
        let s = session();
        s.reset(None, Some("ep".into())).unwrap();
        let step = s
            .step(&action(json!({
                "type": "call_tool",
                "tool_name": "echo_with_length",
                "arguments": {"message": "héllo"}
            })))
            .unwrap();
        assert_eq!(step.reward, Reward::None);
        assert_eq!(step.observation["result"]["data"]["length"], 5);
        assert_eq!(s.snapshot().step_count, 1);
        let mut args = Map::new();
        args.insert("message".into(), json!("hi"));
        assert_eq!(s.call_tool("echo_message", &args).unwrap(), json!("hi"));
        assert_eq!(s.snapshot().step_count, 1);
        assert_eq!(s.state().unwrap()["step_count"], 1);
    }

    #[test]
    fn unknown_tool_is_an_observation_on_the_socket_and_an_error_over_http() {
        let s = session();
        let step = s
            .step(&action(json!({"type": "call_tool", "tool_name": "nope"})))
            .unwrap();
        assert_eq!(step.observation["error"]["error_type"], "tool_not_found");
        match s.call_tool("nope", &Map::new()) {
            Err(senv::ToolCallError::Tool(ToolError::InvalidArgs(message))) => {
                assert_eq!(message, "Unknown tool: nope");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_actions_are_validation_errors() {
        let s = session();
        for bad in [
            json!({}),
            json!({"type": "dance"}),
            json!({"type": "call_tool"}),
            json!({"type": "call_tool", "tool_name": "echo_message", "arguments": 1}),
            json!({"type": "list_tools", "metadata": 1}),
            json!({"type": "list_tools", "extra": true}),
        ] {
            assert_eq!(s.step(&action(bad)).unwrap_err().code, "VALIDATION_ERROR");
        }
        assert_eq!(s.snapshot().step_count, 0);
    }
}
