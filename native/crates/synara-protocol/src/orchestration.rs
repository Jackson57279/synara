//! Read-model shapes: the shell (sidebar) and thread detail streams.
//!
//! Mirrors `packages/contracts/src/orchestration.ts`. Enum-like strings the
//! server may grow (`runtimeMode`, session `status`, activity `tone`) stay
//! `String` here so a newer server never makes a whole snapshot undecodable.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::events::OrchestrationEvent;
use crate::ids::{CommandId, MessageId, ProjectId, SpaceId, ThreadId, TurnId};

/// Fields this client does not model, kept so values round-trip.
pub type Extra = Map<String, Value>;

/// `ModelSelection` after decoding (`ModelSelectionByProvider`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider: String,
    #[serde(default)]
    pub instance_id: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ModelSelection {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        Self {
            instance_id: Some(provider.clone()),
            provider,
            model: model.into(),
            options: None,
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceShell {
    pub id: SpaceId,
    pub name: String,
    pub icon: String,
    pub sort_order: u64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

fn default_project_kind() -> String {
    "project".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectShell {
    pub id: ProjectId,
    #[serde(default = "default_project_kind")]
    pub kind: String,
    pub title: String,
    pub workspace_root: String,
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub space_id: Option<SpaceId>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestTurn {
    pub turn_id: TurnId,
    /// `running | interrupted | completed | error`.
    pub state: String,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub assistant_message_id: Option<MessageId>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl LatestTurn {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub thread_id: ThreadId,
    /// `idle | starting | running | ready | interrupted | stopped | error`.
    pub status: String,
    pub provider_name: Option<String>,
    #[serde(default)]
    pub runtime_mode: Option<String>,
    #[serde(default)]
    pub active_turn_id: Option<TurnId>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Session {
    pub fn is_running(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "running")
    }
}

fn default_interaction_mode() -> String {
    "default".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadShell {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: String,
    #[serde(default = "default_interaction_mode")]
    pub interaction_mode: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    pub latest_turn: Option<LatestTurn>,
    #[serde(default)]
    pub latest_user_message_at: Option<String>,
    #[serde(default)]
    pub has_pending_approvals: Option<bool>,
    #[serde(default)]
    pub has_pending_user_input: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub archived_at: Option<String>,
    pub session: Option<Session>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ThreadShell {
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Whether the sidebar should show this thread as working.
    pub fn is_working(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_running)
            || self
                .latest_turn
                .as_ref()
                .is_some_and(LatestTurn::is_running)
    }

    pub fn needs_attention(&self) -> bool {
        self.has_pending_approvals.unwrap_or(false) || self.has_pending_user_input.unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    pub snapshot_sequence: u64,
    pub spaces: Vec<SpaceShell>,
    pub projects: Vec<ProjectShell>,
    pub threads: Vec<ThreadShell>,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `OrchestrationShellStreamItem`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ShellStreamItem {
    Snapshot {
        snapshot: Box<ShellSnapshot>,
    },
    SpaceUpserted {
        sequence: u64,
        space: SpaceShell,
    },
    SpaceRemoved {
        sequence: u64,
        space_id: SpaceId,
        updated_at: String,
    },
    SpaceOrderUpdated {
        sequence: u64,
        ordered_space_ids: Vec<SpaceId>,
    },
    ProjectUpserted {
        sequence: u64,
        project: Box<ProjectShell>,
    },
    ProjectRemoved {
        sequence: u64,
        project_id: ProjectId,
    },
    ThreadUpserted {
        sequence: u64,
        thread: Box<ThreadShell>,
    },
    ThreadRemoved {
        sequence: u64,
        thread_id: ThreadId,
    },
    /// A kind added by a newer server. Ignored.
    #[serde(other)]
    Unknown,
}

impl ShellStreamItem {
    /// The item's own sequence; `None` for snapshots (they carry `snapshotSequence`).
    pub fn sequence(&self) -> Option<u64> {
        match self {
            Self::SpaceUpserted { sequence, .. }
            | Self::SpaceRemoved { sequence, .. }
            | Self::SpaceOrderUpdated { sequence, .. }
            | Self::ProjectUpserted { sequence, .. }
            | Self::ProjectRemoved { sequence, .. }
            | Self::ThreadUpserted { sequence, .. }
            | Self::ThreadRemoved { sequence, .. } => Some(*sequence),
            Self::Snapshot { .. } | Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSegment {
    pub sequence: u64,
    pub started_at: String,
    pub ended_at: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_segments: Option<Vec<TextSegment>>,
    pub turn_id: Option<TurnId>,
    pub streaming: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedPlan {
    pub id: String,
    pub turn_id: Option<TurnId>,
    pub plan_markdown: String,
    #[serde(default)]
    pub implemented_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub id: String,
    pub tone: String,
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub payload: Value,
    pub turn_id: Option<TurnId>,
    #[serde(default)]
    pub sequence: Option<u64>,
    pub created_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `OrchestrationThread`: the full thread detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    #[serde(default)]
    pub deleted_at: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub proposed_plans: Vec<ProposedPlan>,
    pub activities: Vec<Activity>,
    pub latest_turn: Option<LatestTurn>,
    pub session: Option<Session>,
    /// Server-side settlement state for approvals and questions. When present
    /// it is authoritative: only `pending`/`retryable` entries are actionable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_interactions: Option<Vec<PendingInteraction>>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `OrchestrationPendingInteraction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInteraction {
    /// `approval` or `userInput`.
    pub interaction_kind: String,
    pub request_id: String,
    #[serde(default)]
    pub lifecycle_generation: Option<String>,
    /// `pending`, `responding`, `confirmed`, `retryable`, `uncertain`.
    pub status: String,
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub response_command_id: Option<CommandId>,
    #[serde(default)]
    pub response_requested_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub resolved_at: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailSnapshot {
    pub snapshot_sequence: u64,
    pub thread: Thread,
    #[serde(default)]
    pub history_revision_sequence: Option<u64>,
}

/// `OrchestrationThreadStreamItem`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ThreadStreamItem {
    Snapshot {
        snapshot: Box<ThreadDetailSnapshot>,
    },
    Event {
        event: Box<OrchestrationEvent>,
    },
    Replay {
        events: Vec<OrchestrationEvent>,
    },
    #[serde(other)]
    Unknown,
}

/// `OrchestrationSubscribeThreadInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeThreadInput {
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_replay: Option<bool>,
}

impl SubscribeThreadInput {
    /// Snapshot-first subscription, or a cursor resume with batched replay.
    pub fn new(thread_id: ThreadId, after_sequence: Option<u64>) -> Self {
        Self {
            thread_id,
            batch_replay: after_sequence.map(|_| true),
            after_sequence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn thread_shell_json(id: &str) -> Value {
        json!({
            "id": id,
            "projectId": "p-1",
            "title": "Fix the build",
            "modelSelection": {"provider": "codex", "instanceId": "codex", "model": "gpt-5.5"},
            "runtimeMode": "full-access",
            "branch": null,
            "worktreePath": null,
            "latestTurn": null,
            "createdAt": "2026-11-09T00:00:00.000Z",
            "updatedAt": "2026-11-09T00:00:00.000Z",
            "session": null,
            "somethingNew": {"keep": true}
        })
    }

    #[test]
    fn thread_shell_applies_defaults_and_keeps_unknown_fields() {
        let shell: ThreadShell = serde_json::from_value(thread_shell_json("t-1")).unwrap();
        assert_eq!(shell.interaction_mode, "default");
        assert!(!shell.is_pinned);
        assert!(!shell.is_archived());
        assert!(!shell.is_working());
        assert_eq!(
            shell.extra.get("somethingNew"),
            Some(&json!({"keep": true}))
        );
        let back = serde_json::to_value(&shell).unwrap();
        assert_eq!(back["somethingNew"], json!({"keep": true}));
    }

    #[test]
    fn decodes_every_shell_item_kind() {
        let items = json!([
            {"kind": "snapshot", "snapshot": {"snapshotSequence": 4, "spaces": [], "projects": [], "threads": [thread_shell_json("t-1")], "updatedAt": "x"}},
            {"kind": "thread-upserted", "sequence": 5, "thread": thread_shell_json("t-2")},
            {"kind": "thread-removed", "sequence": 6, "threadId": "t-1"},
            {"kind": "project-removed", "sequence": 7, "projectId": "p-1"},
            {"kind": "space-order-updated", "sequence": 8, "orderedSpaceIds": ["s-1"]},
            {"kind": "space-removed", "sequence": 9, "spaceId": "s-1", "updatedAt": "x"},
            {"kind": "brand-new-kind", "sequence": 10}
        ]);
        let items: Vec<ShellStreamItem> = serde_json::from_value(items).unwrap();
        assert!(
            matches!(&items[0], ShellStreamItem::Snapshot { snapshot } if snapshot.threads.len() == 1)
        );
        assert_eq!(items[1].sequence(), Some(5));
        assert!(
            matches!(&items[2], ShellStreamItem::ThreadRemoved { thread_id, .. } if thread_id.as_str() == "t-1")
        );
        assert_eq!(items[6], ShellStreamItem::Unknown);
    }

    #[test]
    fn subscribe_thread_input_only_batches_on_resume() {
        let fresh = serde_json::to_value(SubscribeThreadInput::new("t".into(), None)).unwrap();
        assert_eq!(fresh, json!({"threadId": "t"}));
        let resume = serde_json::to_value(SubscribeThreadInput::new("t".into(), Some(41))).unwrap();
        assert_eq!(
            resume,
            json!({"threadId": "t", "afterSequence": 41, "batchReplay": true})
        );
    }
}
