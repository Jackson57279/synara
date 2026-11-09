use std::time::Duration;

use serde::Deserialize;
use url::Url;

/// The fields of `GET /health` the supervisor reads. The endpoint is
/// unauthenticated and reports shape-level diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub startup_ready: bool,
    #[serde(default)]
    pub orchestration_subscriptions_ready: bool,
}

/// One readiness probe. `None` when the server is unreachable or the body is
/// not a health snapshot.
pub async fn probe_health(
    http: &reqwest::Client,
    base: &Url,
    timeout: Duration,
) -> Option<HealthSnapshot> {
    let url = base.join("/health").ok()?;
    let response = http.get(url).timeout(timeout).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_tolerates_extra_fields() {
        let snapshot: HealthSnapshot = serde_json::from_str(
            r#"{"status":"ok","startupReady":true,"pushBusReady":true,"orchestrationSubscriptionsReady":false,"projection":{"state":"idle"}}"#,
        )
        .unwrap();
        assert!(snapshot.startup_ready);
        assert!(!snapshot.orchestration_subscriptions_ready);
        let empty: HealthSnapshot = serde_json::from_str("{}").unwrap();
        assert!(!empty.startup_ready);
    }
}
