use serde_json::Value;
use synara_protocol::frames::RpcFailure;
use synara_protocol::negotiate::CompatibilityError;

/// Why the connection ended. Every pending request fails with this.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Disconnect {
    #[error("connection closed by the server")]
    Closed,
    #[error("connection closed by the client")]
    ClientClosed,
    #[error("no pong within the liveness timeout")]
    PongTimeout,
    #[error("websocket error: {0}")]
    Transport(String),
    /// `Defect`: a connection-wide server failure.
    #[error("server defect: {0}")]
    Defect(Value),
    /// `ClientProtocolError`: the server rejected a frame this client sent.
    #[error("server rejected a client frame: {0}")]
    Protocol(Value),
    #[error("malformed server frame: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    /// The RPC ran and failed (typed error, defect, or server-side interrupt).
    #[error("{0}")]
    Failure(RpcFailure),
    #[error("disconnected: {0}")]
    Disconnected(Disconnect),
    /// Opening this stream would exceed the server's per-client limit.
    #[error("stream limit reached for {tag} ({limit} open)")]
    StreamLimit { tag: String, limit: usize },
    #[error("could not decode the response: {0}")]
    Decode(String),
}

impl RpcError {
    /// The typed failure's `code`, such as `ORCHESTRATION_SNAPSHOT_STALLED`.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Failure(failure) => failure.code(),
            _ => None,
        }
    }

    /// Whether a reconnect could make the same request succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Disconnected(_) => true,
            Self::Failure(RpcFailure::Fail(error)) => {
                // `WsRpcError.retryable`; snapshot stalls are explicitly final.
                self.code() != Some("ORCHESTRATION_SNAPSHOT_STALLED")
                    && error
                        .get("retryable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            }
            _ => false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// 426 from negotiate or the upgrade.
    #[error("incompatible server: {0}")]
    Compatibility(CompatibilityError),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("websocket handshake failed: {0}")]
    Ws(String),
    #[error("request failed: {0}")]
    Request(String),
}
