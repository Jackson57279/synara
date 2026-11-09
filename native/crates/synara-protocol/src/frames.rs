//! Effect RPC frames as `RpcSerialization.json` puts them on the wire.
//!
//! One JSON value per WebSocket text message. The server may batch several
//! messages into one JSON array, so [`decode_server_frames`] accepts both.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Per-connection request id. Sent as a JSON number; the server echoes it
/// unchanged, but `RequestId` is `string | number` in Effect so both decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;
        impl Visitor<'_> for IdVisitor {
            type Value = RequestId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a non-negative integer or a decimal string")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<RequestId, E> {
                Ok(RequestId(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<RequestId, E> {
                u64::try_from(v)
                    .map(RequestId)
                    .map_err(|_| E::custom("negative request id"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RequestId, E> {
                v.parse().map(RequestId).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(IdVisitor)
    }
}

/// Messages the client sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "_tag")]
pub enum ClientFrame {
    Request {
        id: RequestId,
        tag: String,
        payload: Value,
        headers: Vec<(String, String)>,
    },
    Ack {
        #[serde(rename = "requestId")]
        request_id: RequestId,
    },
    Interrupt {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        interruptors: Vec<u64>,
    },
    Ping,
    Eof,
}

impl ClientFrame {
    pub fn request(id: RequestId, tag: impl Into<String>, payload: Value) -> Self {
        Self::Request {
            id,
            tag: tag.into(),
            payload,
            headers: Vec::new(),
        }
    }

    pub fn to_json(&self) -> String {
        // Every variant is plain data; serialization cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Messages the server sends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum ServerFrame {
    Chunk {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        values: Vec<Value>,
    },
    Exit {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        exit: Exit,
    },
    /// Connection-wide failure: every pending request fails.
    Defect {
        defect: Value,
    },
    Pong,
    /// The server rejected something the client sent; terminal for all pending requests.
    ClientProtocolError {
        error: Value,
    },
    /// Server-initiated request (unused by this server today; ignored).
    Request {
        #[serde(default)]
        tag: String,
    },
}

/// `ExitEncoded` from `effect/unstable/rpc/RpcMessage`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum Exit {
    Success {
        #[serde(default)]
        value: Value,
    },
    Failure {
        cause: Vec<CauseItem>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum CauseItem {
    Fail {
        error: Value,
    },
    Die {
        #[serde(default)]
        defect: Value,
    },
    Interrupt {
        #[serde(rename = "fiberId", default)]
        fiber_id: Option<u64>,
    },
}

/// A failed `Exit` reduced to what callers act on.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcFailure {
    /// A typed error from the RPC's `error` schema (for example `WsRpcError`).
    Fail(Value),
    /// A server defect (bug); carries the encoded defect.
    Die(Value),
    /// The request was interrupted on the server.
    Interrupted,
}

impl RpcFailure {
    /// The `_tag` of a typed failure, such as `"WsRpcError"`.
    pub fn error_tag(&self) -> Option<&str> {
        match self {
            Self::Fail(error) => error.get("_tag").and_then(Value::as_str),
            _ => None,
        }
    }

    /// The `code` field of a typed failure, such as `"ORCHESTRATION_SNAPSHOT_STALLED"`.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Fail(error) => error.get("code").and_then(Value::as_str),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Fail(error) => error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
            Self::Die(defect) => format!("server defect: {defect}"),
            Self::Interrupted => "interrupted".to_owned(),
        }
    }
}

impl fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code() {
            Some(code) => write!(f, "{code}: {}", self.message()),
            None => f.write_str(&self.message()),
        }
    }
}

impl Exit {
    /// Collapses the cause list: the first `Fail` wins, then `Die`, then `Interrupt`.
    pub fn into_result(self) -> Result<Value, RpcFailure> {
        match self {
            Self::Success { value } => Ok(value),
            Self::Failure { cause } => {
                let mut die = None;
                for item in cause {
                    match item {
                        CauseItem::Fail { error } => return Err(RpcFailure::Fail(error)),
                        CauseItem::Die { defect } => die = die.or(Some(defect)),
                        CauseItem::Interrupt { .. } => {}
                    }
                }
                Err(die.map_or(RpcFailure::Interrupted, RpcFailure::Die))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("malformed server frame: {0}")]
pub struct FrameError(#[from] serde_json::Error);

/// Decodes one WebSocket text message into zero or more server frames.
pub fn decode_server_frames(text: &str) -> Result<Vec<ServerFrame>, FrameError> {
    let value: Value = serde_json::from_str(text)?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| serde_json::from_value(item).map_err(FrameError))
            .collect(),
        other => Ok(vec![serde_json::from_value(other)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_frames_match_effect_encoding() {
        let request = ClientFrame::request(RequestId(7), "orchestration.subscribeShell", json!({}));
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"_tag":"Request","id":7,"tag":"orchestration.subscribeShell","payload":{},"headers":[]})
        );
        assert_eq!(
            ClientFrame::Ack {
                request_id: RequestId(7)
            }
            .to_json(),
            r#"{"_tag":"Ack","requestId":7}"#
        );
        assert_eq!(
            ClientFrame::Interrupt {
                request_id: RequestId(7),
                interruptors: vec![]
            }
            .to_json(),
            r#"{"_tag":"Interrupt","requestId":7,"interruptors":[]}"#
        );
        assert_eq!(ClientFrame::Ping.to_json(), r#"{"_tag":"Ping"}"#);
        assert_eq!(ClientFrame::Eof.to_json(), r#"{"_tag":"Eof"}"#);
    }

    #[test]
    fn decodes_single_and_batched_server_frames() {
        let frames = decode_server_frames(r#"{"_tag":"Pong"}"#).unwrap();
        assert_eq!(frames, vec![ServerFrame::Pong]);

        let frames = decode_server_frames(
            r#"[{"_tag":"Chunk","requestId":"3","values":[1,2]},{"_tag":"Exit","requestId":3,"exit":{"_tag":"Success","value":null}}]"#,
        )
        .unwrap();
        assert_eq!(frames.len(), 2);
        assert!(matches!(
            &frames[0],
            ServerFrame::Chunk { request_id: RequestId(3), values } if values.len() == 2
        ));
    }

    #[test]
    fn failure_prefers_typed_error() {
        let exit: Exit = serde_json::from_value(json!({
            "_tag": "Failure",
            "cause": [
                {"_tag": "Interrupt", "fiberId": 1},
                {"_tag": "Fail", "error": {"_tag": "WsRpcError", "message": "stalled", "code": "ORCHESTRATION_SNAPSHOT_STALLED"}}
            ]
        }))
        .unwrap();
        let failure = exit.into_result().unwrap_err();
        assert_eq!(failure.error_tag(), Some("WsRpcError"));
        assert_eq!(failure.code(), Some("ORCHESTRATION_SNAPSHOT_STALLED"));
        assert_eq!(
            failure.to_string(),
            "ORCHESTRATION_SNAPSHOT_STALLED: stalled"
        );
    }

    #[test]
    fn failure_without_fail_is_die_or_interrupt() {
        let die: Exit = serde_json::from_value(
            json!({"_tag":"Failure","cause":[{"_tag":"Die","defect":"boom"}]}),
        )
        .unwrap();
        assert!(matches!(die.into_result(), Err(RpcFailure::Die(_))));
        let interrupted: Exit =
            serde_json::from_value(json!({"_tag":"Failure","cause":[{"_tag":"Interrupt"}]}))
                .unwrap();
        assert_eq!(interrupted.into_result(), Err(RpcFailure::Interrupted));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_server_frames("not json").is_err());
        assert!(decode_server_frames(r#"{"_tag":"Nope"}"#).is_err());
        assert!(decode_server_frames(r#"{"_tag":"Chunk","requestId":-1,"values":[]}"#).is_err());
    }
}
