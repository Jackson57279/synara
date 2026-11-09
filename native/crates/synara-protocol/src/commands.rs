//! `ClientOrchestrationCommand` variants the native client dispatches, plus
//! `DispatchResult` and `settleTurnDispatch`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CommandId, MessageId, ProjectId, ThreadId, TurnId};
use crate::orchestration::ModelSelection;

/// `RuntimeMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    ApprovalRequired,
    Auto,
    #[default]
    FullAccess,
}

/// `ProviderInteractionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InteractionMode {
    #[default]
    Default,
    Plan,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnMessage {
    pub message_id: MessageId,
    pub role: &'static str,
    pub text: String,
    pub attachments: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ClientCommand {
    #[serde(rename = "project.create", rename_all = "camelCase")]
    ProjectCreate {
        command_id: CommandId,
        project_id: ProjectId,
        title: String,
        workspace_root: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        create_workspace_root_if_missing: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        default_model_selection: Option<ModelSelection>,
        created_at: String,
    },
    #[serde(rename = "thread.create", rename_all = "camelCase")]
    ThreadCreate {
        command_id: CommandId,
        thread_id: ThreadId,
        project_id: ProjectId,
        title: String,
        model_selection: ModelSelection,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
        branch: Option<String>,
        worktree_path: Option<String>,
        created_at: String,
    },
    #[serde(rename = "thread.turn.start", rename_all = "camelCase")]
    TurnStart {
        command_id: CommandId,
        thread_id: ThreadId,
        message: TurnMessage,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_selection: Option<ModelSelection>,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
        created_at: String,
    },
    #[serde(rename = "thread.turn.interrupt", rename_all = "camelCase")]
    TurnInterrupt {
        command_id: CommandId,
        thread_id: ThreadId,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        requested_by: &'static str,
        created_at: String,
    },
}

impl ClientCommand {
    pub fn turn_start(
        thread_id: ThreadId,
        text: impl Into<String>,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> Self {
        Self::TurnStart {
            command_id: CommandId::random(),
            thread_id,
            message: TurnMessage {
                message_id: MessageId::random(),
                role: "user",
                text: text.into(),
                attachments: Vec::new(),
            },
            model_selection: None,
            runtime_mode,
            interaction_mode,
            created_at: iso_now(),
        }
    }

    pub fn thread_create(
        project_id: ProjectId,
        title: impl Into<String>,
        model_selection: ModelSelection,
    ) -> Self {
        Self::ThreadCreate {
            command_id: CommandId::random(),
            thread_id: ThreadId::random(),
            project_id,
            title: title.into(),
            model_selection,
            runtime_mode: RuntimeMode::default(),
            interaction_mode: InteractionMode::default(),
            branch: None,
            worktree_path: None,
            created_at: iso_now(),
        }
    }

    pub fn project_create(title: impl Into<String>, workspace_root: impl Into<String>) -> Self {
        Self::ProjectCreate {
            command_id: CommandId::random(),
            project_id: ProjectId::random(),
            title: title.into(),
            workspace_root: workspace_root.into(),
            create_workspace_root_if_missing: false,
            default_model_selection: None,
            created_at: iso_now(),
        }
    }

    /// Lets the server create a missing workspace root (`project.create` only).
    #[must_use]
    pub fn creating_workspace_root(mut self) -> Self {
        if let Self::ProjectCreate {
            create_workspace_root_if_missing,
            ..
        } = &mut self
        {
            *create_workspace_root_if_missing = true;
        }
        self
    }

    pub fn interrupt(thread_id: ThreadId, turn_id: Option<TurnId>) -> Self {
        Self::TurnInterrupt {
            command_id: CommandId::random(),
            thread_id,
            turn_id,
            requested_by: "user",
            created_at: iso_now(),
        }
    }

    pub fn command_id(&self) -> &CommandId {
        match self {
            Self::ProjectCreate { command_id, .. }
            | Self::ThreadCreate { command_id, .. }
            | Self::TurnStart { command_id, .. }
            | Self::TurnInterrupt { command_id, .. } => command_id,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `DispatchResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DispatchResult {
    pub sequence: u64,
}

/// `OrchestrationSettleTurnDispatchResult`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SettleResult {
    Accepted { sequence: u64 },
    Rejected { message: String },
}

/// `{ command }` payload for `orchestration.settleTurnDispatch`.
pub fn settle_payload(turn_start: &ClientCommand) -> Value {
    serde_json::json!({ "command": turn_start.to_value() })
}

/// `new Date().toISOString()`: UTC, millisecond precision, `Z` suffix.
pub fn iso_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_unix_millis(now.as_millis() as i64)
}

/// Civil-from-days (Howard Hinnant) so the protocol crate needs no date dependency.
pub fn iso_from_unix_millis(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_matches_to_iso_string() {
        assert_eq!(iso_from_unix_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_from_unix_millis(1_762_660_800_123),
            "2025-11-09T04:00:00.123Z"
        );
        assert_eq!(
            iso_from_unix_millis(951_782_400_000),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(iso_now().len(), 24);
    }

    #[test]
    fn turn_start_shape() {
        let command = ClientCommand::turn_start(
            "t-1".into(),
            "hi",
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        );
        let value = command.to_value();
        assert_eq!(value["type"], "thread.turn.start");
        assert_eq!(value["threadId"], "t-1");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["attachments"], json!([]));
        assert_eq!(value["runtimeMode"], "full-access");
        assert_eq!(value["interactionMode"], "default");
        assert!(value.get("modelSelection").is_none());
        assert_eq!(settle_payload(&command)["command"], value);
    }

    #[test]
    fn create_and_interrupt_shapes() {
        let thread = ClientCommand::thread_create(
            "p-1".into(),
            "New",
            ModelSelection::new("codex", "gpt-5.5"),
        )
        .to_value();
        assert_eq!(thread["type"], "thread.create");
        assert_eq!(thread["modelSelection"]["instanceId"], "codex");
        assert_eq!(thread["branch"], Value::Null);

        let project = ClientCommand::project_create("Synara", "/data/home/synara").to_value();
        assert_eq!(project["type"], "project.create");
        assert!(project.get("createWorkspaceRootIfMissing").is_none());

        let stop = ClientCommand::interrupt("t-1".into(), None).to_value();
        assert_eq!(stop["type"], "thread.turn.interrupt");
        assert_eq!(stop["requestedBy"], "user");
    }

    #[test]
    fn settle_result_variants() {
        let accepted: SettleResult =
            serde_json::from_value(json!({"status":"accepted","sequence":9})).unwrap();
        assert_eq!(accepted, SettleResult::Accepted { sequence: 9 });
        let rejected: SettleResult =
            serde_json::from_value(json!({"status":"rejected","message":"no"})).unwrap();
        assert!(matches!(rejected, SettleResult::Rejected { .. }));
    }
}
