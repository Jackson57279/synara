//! Open approvals for a thread: a port of `derivePendingApprovals` in
//! `apps/web/src/pendingInteractionDerivation.ts` without the aggregate
//! fallback (the native client always has the thread detail).
//!
//! Requests replay over activities in provider order, keyed by
//! `requestId` + `lifecycleGeneration`; a resolution closes them; explicit
//! stale-callback failures close them; and when the server sent
//! `pendingInteractions`, only `pending`/`retryable` settlements stay open.

use std::collections::HashSet;

use serde_json::Value;
use synara_protocol::commands::ApprovalDecision;
use synara_protocol::orchestration::{Activity, PendingInteraction, Thread};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub request_id: String,
    pub lifecycle_generation: Option<String>,
    /// `command`, `file-read`, `file-change`, `permissions` or `tool`.
    pub request_kind: &'static str,
    pub created_at: String,
    pub detail: Option<String>,
    pub tool_name: Option<String>,
    /// `false` hides "Accept for session"; absent means available.
    pub session_approval_available: bool,
}

impl PendingApproval {
    /// The card prompt, from `KIND_PROMPT` in
    /// `apps/web/src/components/chat/ComposerPendingApprovalPanel.tsx`.
    pub fn prompt(&self) -> &'static str {
        match self.request_kind {
            "command" => "Approve this command?",
            "file-read" => "Approve reading this file?",
            "file-change" => "Approve this file change?",
            "permissions" => "Grant these permissions?",
            _ => "Approve this tool call?",
        }
    }

    /// `APPROVAL_ACTIONS`, minus "Always allow" when the provider offers no
    /// session-wide approval.
    pub fn actions(&self) -> Vec<(ApprovalDecision, &'static str)> {
        let mut actions = vec![(ApprovalDecision::Accept, "Approve once")];
        if self.session_approval_available {
            actions.push((
                ApprovalDecision::AcceptForSession,
                "Always allow this session",
            ));
        }
        actions.push((ApprovalDecision::Decline, "Decline"));
        actions.push((ApprovalDecision::Cancel, "Cancel turn"));
        actions
    }
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `approvalRequestKindFromRequestType` in `packages/shared/src/threadSummary.ts`.
fn kind_from_request_type(request_type: Option<&str>) -> Option<&'static str> {
    Some(match request_type? {
        "command_execution_approval" | "exec_command_approval" => "command",
        "file_read_approval" => "file-read",
        "file_change_approval" | "apply_patch_approval" => "file-change",
        "permissions_approval" => "permissions",
        "tool_approval" | "dynamic_tool_call" => "tool",
        _ => return None,
    })
}

fn request_kind(payload: &Value) -> Option<&'static str> {
    match text(payload, "requestKind") {
        Some("command") => Some("command"),
        Some("file-read") => Some("file-read"),
        Some("file-change") => Some("file-change"),
        Some("permissions") => Some("permissions"),
        Some("tool") => Some("tool"),
        _ => kind_from_request_type(text(payload, "requestType")),
    }
}

fn is_stale_detail(detail: Option<&str>) -> bool {
    let Some(detail) = detail else {
        return false;
    };
    let d = detail.to_lowercase();
    [
        "stale pending approval request",
        "unknown pending approval request",
        "unknown pending permission request",
    ]
    .iter()
    .any(|needle| d.contains(needle))
}

fn ordered(activities: &[Activity]) -> Vec<&Activity> {
    let mut sorted: Vec<&Activity> = activities.iter().collect();
    sorted.sort_by(|a, b| match (a.sequence, b.sequence) {
        (Some(x), Some(y)) if x != y => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        _ => a.created_at.cmp(&b.created_at),
    });
    sorted
}

fn actionable(settlements: &[PendingInteraction]) -> HashSet<(String, Option<String>)> {
    settlements
        .iter()
        .filter(|s| s.interaction_kind == "approval")
        .filter(|s| matches!(s.status.as_str(), "pending" | "retryable"))
        .map(|s| (s.request_id.clone(), s.lifecycle_generation.clone()))
        .collect()
}

pub fn pending_approvals(thread: &Thread) -> Vec<PendingApproval> {
    let activities = ordered(&thread.activities);
    let mut open: Vec<PendingApproval> = Vec::new();
    for activity in &activities {
        let payload = &activity.payload;
        let Some(request_id) = text(payload, "requestId") else {
            continue;
        };
        let generation = text(payload, "lifecycleGeneration").map(str::to_owned);
        match activity.kind.as_str() {
            "approval.requested" => {
                let Some(kind) = request_kind(payload) else {
                    continue;
                };
                // `replacePendingInteraction`: a new request replaces every
                // earlier instance of the same id.
                open.retain(|p| p.request_id != request_id);
                open.push(PendingApproval {
                    request_id: request_id.to_owned(),
                    lifecycle_generation: generation,
                    request_kind: kind,
                    created_at: activity.created_at.clone(),
                    detail: text(payload, "detail").map(str::to_owned),
                    tool_name: text(payload, "toolName").map(str::to_owned),
                    session_approval_available: payload
                        .get("sessionApprovalAvailable")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                });
            }
            "approval.resolved" => open.retain(|p| {
                p.request_id != request_id
                    || (generation.is_some() && p.lifecycle_generation != generation)
            }),
            _ => {}
        }
    }
    // `createStalePendingInteractionMatcher`, applied after replay.
    for activity in &activities {
        if activity.kind != "provider.approval.respond.failed"
            || !is_stale_detail(text(&activity.payload, "detail"))
        {
            continue;
        }
        let Some(request_id) = text(&activity.payload, "requestId") else {
            continue;
        };
        let generation = text(&activity.payload, "lifecycleGeneration");
        open.retain(|p| {
            let same = p.request_id == request_id;
            let stale = match (generation, p.lifecycle_generation.as_deref()) {
                (Some(g), Some(pg)) => g == pg,
                (None, _) => activity.created_at >= p.created_at,
                (Some(_), None) => false,
            };
            !(same && stale)
        });
    }
    if let Some(settlements) = &thread.pending_interactions {
        let keep = actionable(settlements);
        open.retain(|p| keep.contains(&(p.request_id.clone(), p.lifecycle_generation.clone())));
    }
    open.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(activities: Value, pending: Option<Value>) -> Thread {
        let mut value = json!({
            "id": "t", "projectId": "p", "title": "", "messages": [], "activities": activities,
            "latestTurn": null, "session": null, "createdAt": "0", "updatedAt": "0",
        });
        if let Some(pending) = pending {
            value["pendingInteractions"] = pending;
        }
        serde_json::from_value(value).unwrap()
    }

    fn act(id: &str, kind: &str, at: &str, payload: Value) -> Value {
        json!({"id": id, "kind": kind, "tone": "approval", "summary": kind,
               "payload": payload, "turnId": "u", "createdAt": at})
    }

    #[test]
    fn replays_requests_and_resolutions() {
        let t = thread(
            json!([
                act(
                    "a",
                    "approval.requested",
                    "1",
                    json!({"requestId": "r1", "requestType": "exec_command_approval", "detail": "ls -la"})
                ),
                act(
                    "b",
                    "approval.requested",
                    "2",
                    json!({"requestId": "r2", "requestKind": "tool", "toolName": "browser",
                           "sessionApprovalAvailable": false})
                ),
                act("c", "approval.resolved", "3", json!({"requestId": "r1"})),
                act("d", "approval.requested", "4", json!({"requestId": "r3"})),
            ]),
            None,
        );
        let open = pending_approvals(&t);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].request_id, "r2");
        assert_eq!(open[0].prompt(), "Approve this tool call?");
        assert_eq!(open[0].actions().len(), 3);
    }

    #[test]
    fn settlements_and_stale_failures_close_requests() {
        let activities = json!([
            act(
                "a",
                "approval.requested",
                "1",
                json!({"requestId": "r1", "requestKind": "command"})
            ),
            act(
                "b",
                "approval.requested",
                "2",
                json!({"requestId": "r2", "requestKind": "command"})
            ),
            act(
                "c",
                "approval.requested",
                "3",
                json!({"requestId": "r3", "requestKind": "command"})
            ),
            act(
                "d",
                "provider.approval.respond.failed",
                "4",
                json!({"requestId": "r3", "detail": "Stale pending approval request r3"})
            ),
        ]);
        let settle = |id: &str, status: &str| {
            json!({"interactionKind": "approval", "requestId": id, "lifecycleGeneration": null,
                   "status": status, "createdAt": "1"})
        };
        let t = thread(
            activities,
            Some(json!([
                settle("r1", "responding"),
                settle("r2", "retryable"),
                settle("r3", "pending")
            ])),
        );
        let open: Vec<_> = pending_approvals(&t)
            .into_iter()
            .map(|p| p.request_id)
            .collect();
        assert_eq!(open, vec!["r2"]);
    }
}
