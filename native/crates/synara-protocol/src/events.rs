//! `OrchestrationEvent`: the envelope is typed; payloads are decoded on demand
//! for the event types the native client renders. Every other type still
//! advances the cursor.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CommandId, EventId, MessageId, ThreadId, TurnId};
use crate::orchestration::{Activity, Extra, MessageRole, ProposedPlan, Session};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationEvent {
    pub sequence: u64,
    pub event_id: EventId,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub occurred_at: String,
    pub command_id: Option<CommandId>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSentPayload {
    pub thread_id: ThreadId,
    pub message_id: MessageId,
    pub role: MessageRole,
    pub text: String,
    #[serde(default)]
    pub segment_started_at: Option<String>,
    #[serde(default)]
    pub segment_sequence: Option<u64>,
    pub turn_id: Option<TurnId>,
    pub streaming: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityAppendedPayload {
    pub thread_id: ThreadId,
    pub activity: Activity,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetPayload {
    pub thread_id: ThreadId,
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedPlanUpsertedPayload {
    pub thread_id: ThreadId,
    pub proposed_plan: ProposedPlan,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaUpdatedPayload {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDeletedPayload {
    pub thread_id: ThreadId,
    pub deleted_at: String,
}

/// `thread.approval-response-requested` / `thread.user-input-response-requested`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseRequestedPayload {
    pub thread_id: ThreadId,
    pub request_id: String,
    #[serde(default)]
    pub lifecycle_generation: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    pub created_at: String,
}

/// The event types the transcript reacts to.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadEvent {
    MessageSent(Box<MessageSentPayload>),
    ActivityAppended(Box<ActivityAppendedPayload>),
    SessionSet(Box<SessionSetPayload>),
    ProposedPlanUpserted(Box<ProposedPlanUpsertedPayload>),
    MetaUpdated(MetaUpdatedPayload),
    /// The server accepted a response to an approval (`approval: true`) or a
    /// question; the matching pending interaction is now `responding`.
    ResponseRequested {
        approval: bool,
        command_id: Option<CommandId>,
        payload: ResponseRequestedPayload,
    },
    Deleted(ThreadDeletedPayload),
    /// `thread.reverted` / `thread.conversation-rolled-back`: history changed
    /// underneath the client; the thread must be re-snapshotted.
    HistoryRewritten {
        thread_id: Option<ThreadId>,
    },
    /// Anything else: advances the cursor only.
    Other,
}

pub mod types {
    pub const MESSAGE_SENT: &str = "thread.message-sent";
    pub const ACTIVITY_APPENDED: &str = "thread.activity-appended";
    pub const SESSION_SET: &str = "thread.session-set";
    pub const PROPOSED_PLAN_UPSERTED: &str = "thread.proposed-plan-upserted";
    pub const META_UPDATED: &str = "thread.meta-updated";
    pub const DELETED: &str = "thread.deleted";
    pub const APPROVAL_RESPONSE_REQUESTED: &str = "thread.approval-response-requested";
    pub const USER_INPUT_RESPONSE_REQUESTED: &str = "thread.user-input-response-requested";
    pub const REVERTED: &str = "thread.reverted";
    pub const ROLLED_BACK: &str = "thread.conversation-rolled-back";
}

impl OrchestrationEvent {
    /// Decodes the payload for the types the transcript renders.
    pub fn decode(&self) -> Result<ThreadEvent, serde_json::Error> {
        fn parse<T: serde::de::DeserializeOwned>(payload: &Value) -> Result<T, serde_json::Error> {
            T::deserialize(payload)
        }
        Ok(match self.event_type.as_str() {
            types::MESSAGE_SENT => ThreadEvent::MessageSent(Box::new(parse(&self.payload)?)),
            types::ACTIVITY_APPENDED => {
                ThreadEvent::ActivityAppended(Box::new(parse(&self.payload)?))
            }
            types::SESSION_SET => ThreadEvent::SessionSet(Box::new(parse(&self.payload)?)),
            types::PROPOSED_PLAN_UPSERTED => {
                ThreadEvent::ProposedPlanUpserted(Box::new(parse(&self.payload)?))
            }
            types::META_UPDATED => ThreadEvent::MetaUpdated(parse(&self.payload)?),
            types::DELETED => ThreadEvent::Deleted(parse(&self.payload)?),
            kind @ (types::APPROVAL_RESPONSE_REQUESTED | types::USER_INPUT_RESPONSE_REQUESTED) => {
                ThreadEvent::ResponseRequested {
                    approval: kind == types::APPROVAL_RESPONSE_REQUESTED,
                    command_id: self.command_id.clone(),
                    payload: parse(&self.payload)?,
                }
            }
            types::REVERTED | types::ROLLED_BACK => ThreadEvent::HistoryRewritten {
                thread_id: self
                    .payload
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(ThreadId::from),
            },
            _ => ThreadEvent::Other,
        })
    }

    /// The thread this event belongs to, when it is a thread event.
    pub fn thread_id(&self) -> Option<ThreadId> {
        (self.aggregate_kind == "thread").then(|| ThreadId::from(self.aggregate_id.as_str()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn event_json(sequence: u64, event_type: &str, payload: Value) -> Value {
        json!({
            "sequence": sequence,
            "eventId": format!("e-{sequence}"),
            "aggregateKind": "thread",
            "aggregateId": "t-1",
            "occurredAt": "2026-11-09T00:00:00.000Z",
            "commandId": null,
            "causationEventId": null,
            "correlationId": null,
            "metadata": {},
            "type": event_type,
            "payload": payload
        })
    }

    #[test]
    fn decodes_message_sent() {
        let event: OrchestrationEvent = serde_json::from_value(event_json(
            12,
            types::MESSAGE_SENT,
            json!({
                "threadId": "t-1", "messageId": "m-1", "role": "assistant", "text": "Hel",
                "turnId": "turn-1", "streaming": true,
                "createdAt": "x", "updatedAt": "x", "source": "native"
            }),
        ))
        .unwrap();
        assert_eq!(event.thread_id(), Some(ThreadId::from("t-1")));
        let ThreadEvent::MessageSent(payload) = event.decode().unwrap() else {
            panic!("expected message-sent");
        };
        assert_eq!(payload.text, "Hel");
        assert!(payload.streaming);
        assert_eq!(payload.extra.get("source"), Some(&json!("native")));
        // Envelope fields the client does not model survive a round trip.
        assert!(serde_json::to_value(&event)
            .unwrap()
            .get("metadata")
            .is_some());
    }

    #[test]
    fn unknown_types_are_other_and_rewrites_are_flagged() {
        let other: OrchestrationEvent =
            serde_json::from_value(event_json(1, "thread.something-new", json!({"x": 1}))).unwrap();
        assert_eq!(other.decode().unwrap(), ThreadEvent::Other);
        let reverted: OrchestrationEvent =
            serde_json::from_value(event_json(2, types::REVERTED, json!({"threadId": "t-1"})))
                .unwrap();
        assert_eq!(
            reverted.decode().unwrap(),
            ThreadEvent::HistoryRewritten {
                thread_id: Some("t-1".into())
            }
        );
    }

    #[test]
    fn malformed_known_payload_is_an_error() {
        let bad: OrchestrationEvent = serde_json::from_value(event_json(
            3,
            types::SESSION_SET,
            json!({"threadId": "t-1"}),
        ))
        .unwrap();
        assert!(bad.decode().is_err());
    }
}
