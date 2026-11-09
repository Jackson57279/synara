use serde::Deserialize;
use serde_json::{Map, Value};

/// `instabox get --json` / `ls --json` / `create --json`. Typed fields are
/// the ones the client reads; the rest is kept in `extra`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    pub id: String,
    pub name: String,
    /// `running`, `suspended`, `stopped`, `starting`, ... (string-typed: the
    /// CLI adds states over time).
    pub state: String,
    #[serde(default)]
    pub desired_state: String,
    #[serde(default)]
    pub spec: Option<SandboxSpec>,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub guest_user: Option<String>,
    #[serde(default)]
    pub max_run_minutes: Option<u32>,
    #[serde(default)]
    pub auto_suspend_after_seconds: Option<u64>,
    #[serde(default)]
    pub ready_at: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Sandbox {
    /// SSH, exec and files work.
    pub fn is_ready(&self) -> bool {
        self.state == "running" && self.runtime.as_ref().is_some_and(|r| r.ready)
    }

    /// `metadata.setupStatus` from `create --setup-file`.
    pub fn setup_status(&self) -> Option<&str> {
        self.metadata.get("setupStatus").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    pub vcpus: u32,
    #[serde(rename = "memMiB")]
    pub mem_mib: u32,
    #[serde(default, rename = "persistentDiskGiB")]
    pub persistent_disk_gib: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Runtime {
    #[serde(default)]
    pub ready: bool,
}

/// `instabox exec --json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// `None` when the command died on a signal.
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub oom_killed: bool,
    #[serde(default)]
    pub duration_ms: u64,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.oom_killed
    }
}

/// One entry of `instabox secrets ls --json`. Values are never listed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMeta {
    pub name: String,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SecretList {
    #[serde(default)]
    pub items: Vec<SecretMeta>,
}

/// `{"error": {"code", "message"}}`, which the CLI prints with `--json` on
/// failure.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CliError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CliErrorEnvelope {
    pub error: CliError,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GET: &str = r#"{
      "id": "m_agq35", "name": "synara-native", "desiredState": "running",
      "state": "running", "revision": 1,
      "spec": {"vcpus": 4, "memMiB": 8192, "persistentDiskGiB": 20, "rootDiskGiB": 16},
      "runtime": {"ready": true}, "maxRunMinutes": 120,
      "metadata": {"setupStatus": "succeeded"}, "guestUser": "dih",
      "autoSuspendAfterSeconds": 1800, "oomKills": 0
    }"#;

    #[test]
    fn decodes_get_and_keeps_unknown_fields() {
        let sandbox: Sandbox = serde_json::from_str(GET).unwrap();
        assert!(sandbox.is_ready());
        assert_eq!(sandbox.setup_status(), Some("succeeded"));
        assert_eq!(sandbox.spec.unwrap().mem_mib, 8192);
        assert_eq!(sandbox.extra["oomKills"], 0);
        let suspended: Sandbox =
            serde_json::from_str(r#"{"id":"m","name":"n","state":"suspended"}"#).unwrap();
        assert!(!suspended.is_ready());
    }

    #[test]
    fn decodes_exec() {
        let result: ExecResult = serde_json::from_str(
            r#"{"stdout":"out\n","stderr":"err\n","exitCode":7,"timedOut":false,"truncated":false,"oomKilled":false,"durationMs":434}"#,
        )
        .unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert!(!result.success());
    }
}
