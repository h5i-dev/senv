//! Pure Echo transitions.
//!
//! `transition` is deterministic. The shell supplies the episode id and the
//! clock is not read here. Tool arguments arrive already parsed; this module
//! decides the next snapshot, the reward and whether the episode is done.
//!
//! Echo never terminates. Reset's reward is zero. A step's reward is absent.
//! Those are properties of this function, not of the WebSocket server.

use serde_json::Value;

/// One session's actor. Every Echo engine uses the same tenant id; isolation
/// is a separate engine per session, which this function does not establish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant: u64,
}

pub const SESSION_TENANT: u64 = 1;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub episode_id: String,
    pub step_count: u64,
    pub reset_count: u64,
    /// Accepted and stored. Echo does not consult it.
    pub seed: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Reset {
        seed: Option<i64>,
        episode_id: String,
    },
    /// WebSocket `step`. Increments `step_count` on success.
    Step(StepAction),
    /// HTTP `tools/call` and `tools/list`. Upstream does not count these as steps.
    Invoke(StepAction),
    State,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StepAction {
    ListTools,
    Call(ToolCall),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolCall {
    EchoMessage {
        message: Value,
    },
    EchoWithLength {
        message: String,
    },
    Failed {
        name: String,
        error_type: ToolErrorType,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolErrorType {
    Execution,
    InvalidArgs,
    NotFound,
}

impl ToolErrorType {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Execution => "execution_error",
            Self::InvalidArgs => "invalid_args",
            Self::NotFound => "tool_not_found",
        }
    }
}

/// Reward in thousandths. `None` is JSON null. `Some(0)` is `0.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub body: Body,
    pub reward_milli: Option<i64>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Ready,
    Tools,
    Called {
        name: String,
        output: Value,
    },
    Failed {
        name: String,
        error_type: ToolErrorType,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Outcome(Outcome),
    State { episode_id: String, step_count: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EchoError {
    StepCountOverflow,
}

pub struct EchoKernel;

pub fn tool_specs() -> &'static [(&'static str, &'static str)] {
    &[
        ("echo_message", "Echo back the provided message."),
        ("echo_with_length", "Echo back the message with its length."),
    ]
}

pub fn transition(
    actor: &Principal,
    snap: &Snapshot,
    cmd: &Command,
) -> Result<(Snapshot, Reply), EchoError> {
    let _ = actor;
    match cmd {
        Command::Reset { seed, episode_id } => {
            let next = Snapshot {
                episode_id: episode_id.clone(),
                step_count: 0,
                reset_count: snap.reset_count.saturating_add(1),
                seed: *seed,
            };
            Ok((
                next,
                Reply::Outcome(Outcome {
                    body: Body::Ready,
                    reward_milli: Some(0),
                    done: false,
                }),
            ))
        }
        Command::Step(action) => {
            let step_count = snap
                .step_count
                .checked_add(1)
                .ok_or(EchoError::StepCountOverflow)?;
            let body = eval(action)?;
            let next = Snapshot {
                step_count,
                ..snap.clone()
            };
            Ok((
                next,
                Reply::Outcome(Outcome {
                    body,
                    reward_milli: None,
                    done: false,
                }),
            ))
        }
        Command::Invoke(action) => {
            let body = eval(action)?;
            Ok((
                snap.clone(),
                Reply::Outcome(Outcome {
                    body,
                    reward_milli: None,
                    done: false,
                }),
            ))
        }
        Command::State => Ok((
            snap.clone(),
            Reply::State {
                episode_id: snap.episode_id.clone(),
                step_count: snap.step_count,
            },
        )),
    }
}

fn eval(action: &StepAction) -> Result<Body, EchoError> {
    Ok(match action {
        StepAction::ListTools => Body::Tools,
        StepAction::Call(ToolCall::EchoMessage { message }) => Body::Called {
            name: "echo_message".to_string(),
            output: message.clone(),
        },
        StepAction::Call(ToolCall::EchoWithLength { message }) => {
            let length =
                u64::try_from(message.chars().count()).map_err(|_| EchoError::StepCountOverflow)?;
            Body::Called {
                name: "echo_with_length".to_string(),
                output: serde_json::json!({ "message": message, "length": length }),
            }
        }
        StepAction::Call(ToolCall::Failed {
            name,
            error_type,
            message,
        }) => Body::Failed {
            name: name.clone(),
            error_type: *error_type,
            message: message.clone(),
        },
    })
}

/// The write set is the next snapshot. `apply` does not merge.
pub fn apply(_snap: &Snapshot, next: &Snapshot) -> Snapshot {
    next.clone()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn snap(id: &str, steps: u64) -> Snapshot {
        Snapshot {
            episode_id: id.to_string(),
            step_count: steps,
            reset_count: 0,
            seed: None,
        }
    }

    #[test]
    fn reset_zeroes_steps_and_rewards_zero() {
        let actor = Principal {
            tenant: SESSION_TENANT,
        };
        let before = snap("old", 4);
        let (next, reply) = transition(
            &actor,
            &before,
            &Command::Reset {
                seed: Some(7),
                episode_id: "ep".to_string(),
            },
        )
        .unwrap();
        assert_eq!(next.episode_id, "ep");
        assert_eq!(next.step_count, 0);
        assert_eq!(next.seed, Some(7));
        assert_eq!(next.reset_count, 1);
        match reply {
            Reply::Outcome(o) => {
                assert_eq!(o.body, Body::Ready);
                assert_eq!(o.reward_milli, Some(0));
                assert!(!o.done);
            }
            Reply::State { .. } => panic!("reset returns an observation"),
        }
        assert_eq!(before.step_count, 4, "the previous snapshot is not mutated");
    }

    #[test]
    fn seed_does_not_change_the_ready_observation() {
        let actor = Principal { tenant: 1 };
        let s = snap("a", 0);
        let (_, a) = transition(
            &actor,
            &s,
            &Command::Reset {
                seed: None,
                episode_id: "x".into(),
            },
        )
        .unwrap();
        let (_, b) = transition(
            &actor,
            &s,
            &Command::Reset {
                seed: Some(-3),
                episode_id: "x".into(),
            },
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn step_increments_and_invoke_does_not() {
        let actor = Principal { tenant: 1 };
        let s = snap("ep", 2);
        let call = StepAction::Call(ToolCall::EchoMessage {
            message: Value::String("hi".into()),
        });
        let (stepped, _) = transition(&actor, &s, &Command::Step(call.clone())).unwrap();
        assert_eq!(stepped.step_count, 3);
        assert_eq!(stepped.episode_id, "ep");
        let (same, reply) = transition(&actor, &s, &Command::Invoke(call)).unwrap();
        assert_eq!(same, s);
        match reply {
            Reply::Outcome(o) => match o.body {
                Body::Called { output, .. } => assert_eq!(output, Value::String("hi".into())),
                other => panic!("unexpected {other:?}"),
            },
            Reply::State { .. } => panic!("invoke returns an observation"),
        }
    }

    #[test]
    fn length_counts_unicode_scalars() {
        let actor = Principal { tenant: 1 };
        let (_, reply) = transition(
            &actor,
            &snap("ep", 0),
            &Command::Step(StepAction::Call(ToolCall::EchoWithLength {
                message: "héllo".into(),
            })),
        )
        .unwrap();
        match reply {
            Reply::Outcome(o) => match o.body {
                Body::Called { output, .. } => {
                    assert_eq!(output["length"], 5);
                    assert_eq!(output["message"], "héllo");
                }
                other => panic!("unexpected {other:?}"),
            },
            Reply::State { .. } => panic!("step returns an observation"),
        }
    }

    #[test]
    fn state_is_a_copy() {
        let actor = Principal { tenant: 1 };
        let s = snap("ep", 9);
        let (next, reply) = transition(&actor, &s, &Command::State).unwrap();
        assert_eq!(next, s);
        assert_eq!(apply(&s, &next), s);
        assert_eq!(
            reply,
            Reply::State {
                episode_id: "ep".into(),
                step_count: 9
            }
        );
    }

    #[test]
    fn failed_tool_still_counts_a_step() {
        let actor = Principal { tenant: 1 };
        let (next, reply) = transition(
            &actor,
            &snap("ep", 0),
            &Command::Step(StepAction::Call(ToolCall::Failed {
                name: "missing".into(),
                error_type: ToolErrorType::NotFound,
                message: "Unknown tool: missing".into(),
            })),
        )
        .unwrap();
        assert_eq!(next.step_count, 1);
        match reply {
            Reply::Outcome(o) => {
                assert!(o.reward_milli.is_none());
                assert!(!o.done);
            }
            Reply::State { .. } => panic!("step returns an observation"),
        }
    }
}
