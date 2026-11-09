//! `GET /ws/negotiate` and the `/ws` upgrade URL.
//!
//! Mirrors `packages/contracts/src/wsCompatibility.ts` and
//! `apps/server/src/wsCompatibility.ts`.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::generated::{MAX_REVISION, MIN_REVISION, PROTOCOL_EPOCH, REQUIRED_CAPABILITIES};

pub const NEGOTIATE_PATH: &str = "/ws/negotiate";
pub const WS_PATH: &str = "/ws";

pub mod query {
    pub const CLIENT_BUILD: &str = "x-synara-client-build";
    pub const PROTOCOL_EPOCH: &str = "x-synara-protocol-epoch";
    pub const MIN_REVISION: &str = "x-synara-protocol-min-revision";
    pub const MAX_REVISION: &str = "x-synara-protocol-max-revision";
    pub const REQUIRED_CAPABILITY: &str = "x-synara-required-capability";
    pub const PROTOCOL_REVISION: &str = "x-synara-protocol-revision";
    pub const SERVER_INSTANCE: &str = "x-synara-server-instance";
    pub const TOKEN: &str = "token";
}

/// `WsBootstrapNegotiateResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NegotiateResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_attachment_id: Option<String>,
    pub protocol_epoch: i64,
    pub negotiated_revision: u32,
    pub server_build: String,
    pub server_instance_id: String,
    pub capabilities: Vec<String>,
}

impl NegotiateResult {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompatibilityCode {
    WsProtocolIncompatible,
    WsCapabilitiesIncompatible,
    WsNegotiationRequired,
    WsServerGenerationChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityAction {
    Reload,
    UpdateClient,
    UpdateServer,
}

/// `WsCompatibilityError` (426 body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{code:?} ({action:?}): {message}")]
pub struct CompatibilityError {
    pub message: String,
    pub code: CompatibilityCode,
    pub action: CompatibilityAction,
    pub server_build: String,
    pub protocol_epoch: i64,
    pub min_revision: u32,
    pub max_revision: u32,
}

impl CompatibilityError {
    /// Whether reconnecting with a fresh negotiate can fix this without user action.
    pub fn renegotiate_fixes(&self) -> bool {
        matches!(
            self.code,
            CompatibilityCode::WsServerGenerationChanged | CompatibilityCode::WsNegotiationRequired
        )
    }
}

/// What the client offers during negotiate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOffer {
    pub client_build: String,
    pub min_revision: u32,
    pub max_revision: u32,
    pub required_capabilities: Vec<String>,
}

impl ClientOffer {
    /// The offer this build of the native client makes.
    pub fn native(version: &str) -> Self {
        Self {
            client_build: format!("native/{version}"),
            min_revision: MIN_REVISION,
            max_revision: MAX_REVISION,
            required_capabilities: REQUIRED_CAPABILITIES
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
        }
    }

    /// `GET /ws/negotiate?...` on the server's HTTP origin.
    pub fn negotiate_url(&self, base: &Url) -> Url {
        let mut url = base.clone();
        url.set_path(NEGOTIATE_PATH);
        url.set_query(None);
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair(query::PROTOCOL_EPOCH, &PROTOCOL_EPOCH.to_string());
            pairs.append_pair(query::MIN_REVISION, &self.min_revision.to_string());
            pairs.append_pair(query::MAX_REVISION, &self.max_revision.to_string());
            pairs.append_pair(query::CLIENT_BUILD, &self.client_build);
            for capability in &self.required_capabilities {
                pairs.append_pair(query::REQUIRED_CAPABILITY, capability);
            }
        }
        url
    }

    /// The `ws://` / `wss://` upgrade URL after a successful negotiate.
    pub fn connect_url(
        &self,
        base: &Url,
        negotiated: &NegotiateResult,
        token: Option<&str>,
    ) -> Url {
        let mut url = base.clone();
        let scheme = if base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        // set_scheme only fails between special and non-special schemes; http(s)→ws(s) is fine.
        let _ = url.set_scheme(scheme);
        url.set_path(WS_PATH);
        url.set_query(None);
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair(query::PROTOCOL_EPOCH, &PROTOCOL_EPOCH.to_string());
            pairs.append_pair(
                query::PROTOCOL_REVISION,
                &negotiated.negotiated_revision.to_string(),
            );
            pairs.append_pair(query::CLIENT_BUILD, &self.client_build);
            pairs.append_pair(query::SERVER_INSTANCE, &negotiated.server_instance_id);
            if let Some(token) = token {
                pairs.append_pair(query::TOKEN, token);
            }
        }
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn negotiated() -> NegotiateResult {
        serde_json::from_value(json!({
            "protocolEpoch": 1,
            "negotiatedRevision": 3,
            "serverBuild": "0.9.3",
            "serverInstanceId": "inst-1",
            "capabilities": ["transport.http-negotiate", "rpc.typed-errors"]
        }))
        .unwrap()
    }

    #[test]
    fn negotiate_url_repeats_capabilities() {
        let offer = ClientOffer::native("0.1.0");
        let url = offer.negotiate_url(&Url::parse("http://127.0.0.1:3773/some/path?x=1").unwrap());
        assert_eq!(url.path(), "/ws/negotiate");
        let caps: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == query::REQUIRED_CAPABILITY)
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(caps.len(), REQUIRED_CAPABILITIES.len());
        assert!(url
            .query()
            .unwrap()
            .contains("x-synara-client-build=native%2F0.1.0"));
        assert!(!url.query().unwrap().contains("x=1"));
    }

    #[test]
    fn connect_url_switches_scheme_and_carries_instance() {
        let offer = ClientOffer::native("0.1.0");
        let plain = offer.connect_url(
            &Url::parse("http://127.0.0.1:3773").unwrap(),
            &negotiated(),
            Some("tok"),
        );
        assert_eq!(plain.scheme(), "ws");
        assert_eq!(plain.path(), "/ws");
        let q = plain.query().unwrap();
        assert!(q.contains("x-synara-server-instance=inst-1"));
        assert!(q.contains("x-synara-protocol-revision=3"));
        assert!(q.contains("token=tok"));

        let tls = offer.connect_url(
            &Url::parse("https://box.example").unwrap(),
            &negotiated(),
            None,
        );
        assert_eq!(tls.scheme(), "wss");
        assert!(!tls.query().unwrap().contains("token="));
    }

    #[test]
    fn decodes_compatibility_error() {
        let error: CompatibilityError = serde_json::from_value(json!({
            "_tag": "WsCompatibilityError",
            "message": "server restarted",
            "code": "WS_SERVER_GENERATION_CHANGED",
            "retryable": false,
            "action": "reload",
            "serverBuild": "0.9.3",
            "protocolEpoch": 1,
            "minRevision": 3,
            "maxRevision": 3
        }))
        .unwrap();
        assert!(error.renegotiate_fixes());
        assert_eq!(error.action, CompatibilityAction::Reload);
        assert!(negotiated().has_capability("rpc.typed-errors"));
    }
}
