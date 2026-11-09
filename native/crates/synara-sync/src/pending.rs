//! Keeps `thread.pendingInteractions` current between snapshots, mirroring
//! `reconcilePendingInteractionsFromActivity` and `markInteractionResponding`
//! in `apps/web/src/storeEventReducer.ts`.
//!
//! Differences from the web: a `responding` interaction is only re-claimable
//! when it has no `responseRequestedAt` (the web also reclaims after a 30 s
//! grace); the next snapshot settles any drift.

use serde_json::Value;
use synara_protocol::events::ResponseRequestedPayload;
use synara_protocol::ids::{CommandId, ThreadId};
use synara_protocol::orchestration::{Activity, Extra, PendingInteraction};

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `isStalePendingRequestFailureDetail` in `packages/shared/src/threadSummary.ts`.
fn is_stale_detail(detail: Option<&str>) -> bool {
    let Some(detail) = detail else {
        return false;
    };
    let normalized = detail.to_lowercase();
    [
        "stale pending approval request",
        "stale pending user-input request",
        "unknown pending approval request",
        "unknown pending permission request",
        "unknown pending user-input request",
        "stale pending user input request",
        "unknown pending user input request",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

enum Kind {
    Requested,
    Resolved,
    RespondFailed,
}

fn classify(kind: &str) -> Option<(&'static str, Kind)> {
    Some(match kind {
        "approval.requested" => ("approval", Kind::Requested),
        "approval.resolved" => ("approval", Kind::Resolved),
        "provider.approval.respond.failed" => ("approval", Kind::RespondFailed),
        "user-input.requested" => ("userInput", Kind::Requested),
        "user-input.resolved" => ("userInput", Kind::Resolved),
        "provider.user-input.respond.failed" => ("userInput", Kind::RespondFailed),
        _ => return None,
    })
}

/// Applies one appended activity. Returns whether the list changed.
pub fn reconcile_activity(
    thread_id: &ThreadId,
    pending: &mut Option<Vec<PendingInteraction>>,
    activity: &Activity,
) -> bool {
    let Some((interaction_kind, kind)) = classify(&activity.kind) else {
        return false;
    };
    let payload = &activity.payload;
    let Some(request_id) = text(payload, "requestId") else {
        return false;
    };
    let generation = text(payload, "lifecycleGeneration").map(str::to_owned);
    let matches = |i: &PendingInteraction| {
        i.interaction_kind == interaction_kind
            && i.request_id == request_id
            && (generation.is_none() || i.lifecycle_generation == generation)
    };

    match kind {
        Kind::Resolved => {
            let Some(list) = pending.as_mut() else {
                return false;
            };
            let before = list.len();
            list.retain(|i| !matches(i));
            list.len() != before
        }
        Kind::RespondFailed => {
            let Some(list) = pending.as_mut() else {
                return false;
            };
            let response_command = text(payload, "responseCommandId");
            let stale_detail = is_stale_detail(text(payload, "detail"));
            // `createStalePendingInteractionMatcher` over this one activity.
            let is_stale = |i: &PendingInteraction| {
                stale_detail
                    && match (&generation, &i.lifecycle_generation) {
                        (Some(_), Some(_)) => true,
                        (None, _) => activity.created_at >= i.created_at,
                        (Some(_), None) => false,
                    }
            };
            let settlement = if text(payload, "settlementStatus") == Some("retryable") {
                "retryable"
            } else {
                "uncertain"
            };
            let mut changed = false;
            for i in list.iter_mut() {
                if !matches(i) || i.status == "confirmed" {
                    continue;
                }
                let eligible = match response_command {
                    Some(command) => {
                        i.status == "responding"
                            && i.response_command_id.as_ref().map(CommandId::as_str)
                                == Some(command)
                    }
                    None => is_stale(i),
                };
                if !eligible {
                    continue;
                }
                changed = true;
                if is_stale(i) {
                    i.status = "confirmed".into();
                    i.resolved_at = Some(activity.created_at.clone());
                } else {
                    i.status = settlement.into();
                    i.resolved_at = None;
                }
            }
            changed
        }
        Kind::Requested => {
            let list = pending.get_or_insert_with(Vec::new);
            let at = list
                .iter()
                .position(|i| i.interaction_kind == interaction_kind && i.request_id == request_id);
            if let Some(current) = at.map(|ix| &list[ix]) {
                if current.lifecycle_generation == generation
                    && matches!(
                        current.status.as_str(),
                        "responding" | "confirmed" | "uncertain"
                    )
                {
                    return false;
                }
            }
            let created_at = match at.map(|ix| &list[ix]) {
                Some(current) if current.lifecycle_generation == generation => {
                    current.created_at.clone()
                }
                _ => activity.created_at.clone(),
            };
            let mut extra = Extra::new();
            extra.insert("threadId".into(), Value::String(thread_id.to_string()));
            let entry = PendingInteraction {
                interaction_kind: interaction_kind.into(),
                request_id: request_id.into(),
                lifecycle_generation: generation,
                status: "pending".into(),
                turn_id: activity.turn_id.clone(),
                decision: None,
                response_command_id: None,
                response_requested_at: None,
                created_at,
                resolved_at: None,
                extra,
            };
            match at {
                Some(ix) => list[ix] = entry,
                None => list.push(entry),
            }
            true
        }
    }
}

/// `markInteractionResponding`. Returns whether the list changed.
pub fn mark_responding(
    pending: &mut Option<Vec<PendingInteraction>>,
    approval: bool,
    command_id: Option<&CommandId>,
    payload: &ResponseRequestedPayload,
) -> bool {
    let (Some(list), Some(command_id)) = (pending.as_mut(), command_id) else {
        return false;
    };
    let kind = if approval { "approval" } else { "userInput" };
    let mut changed = false;
    for i in list.iter_mut() {
        let claimable = match i.status.as_str() {
            "pending" | "retryable" | "uncertain" => true,
            "responding" => i.response_requested_at.is_none(),
            _ => false,
        };
        if i.interaction_kind != kind
            || i.request_id != payload.request_id
            || i.lifecycle_generation != payload.lifecycle_generation
            || !claimable
        {
            continue;
        }
        changed = true;
        i.status = "responding".into();
        i.decision = if approval {
            payload.decision.clone()
        } else {
            None
        };
        i.response_command_id = Some(command_id.clone());
        i.response_requested_at = Some(payload.created_at.clone());
        i.resolved_at = None;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(kind: &str, at: &str, payload: Value) -> Activity {
        serde_json::from_value(json!({
            "id": format!("{kind}-{at}"), "kind": kind, "tone": "approval", "summary": kind,
            "payload": payload, "turnId": "turn-1", "createdAt": at,
        }))
        .unwrap()
    }

    #[test]
    fn request_respond_fail_and_resolve() {
        let thread = ThreadId::from("t");
        let mut list = None;
        let req = activity("approval.requested", "1", json!({"requestId": "r1"}));
        assert!(reconcile_activity(&thread, &mut list, &req));
        assert_eq!(list.as_ref().unwrap()[0].status, "pending");

        let command = CommandId::from("c1");
        let respond = ResponseRequestedPayload {
            thread_id: thread.clone(),
            request_id: "r1".into(),
            lifecycle_generation: None,
            decision: Some("accept".into()),
            created_at: "2".into(),
        };
        assert!(mark_responding(&mut list, true, Some(&command), &respond));
        assert_eq!(list.as_ref().unwrap()[0].status, "responding");
        // A repeated request while responding is ignored.
        assert!(!reconcile_activity(&thread, &mut list, &req));

        let failed = activity(
            "provider.approval.respond.failed",
            "3",
            json!({"requestId": "r1", "responseCommandId": "c1", "settlementStatus": "retryable"}),
        );
        assert!(reconcile_activity(&thread, &mut list, &failed));
        assert_eq!(list.as_ref().unwrap()[0].status, "retryable");

        let resolved = activity("approval.resolved", "4", json!({"requestId": "r1"}));
        assert!(reconcile_activity(&thread, &mut list, &resolved));
        assert!(list.unwrap().is_empty());
    }

    #[test]
    fn stale_failure_confirms() {
        let thread = ThreadId::from("t");
        let mut list = None;
        reconcile_activity(
            &thread,
            &mut list,
            &activity("user-input.requested", "1", json!({"requestId": "q"})),
        );
        let stale = activity(
            "provider.user-input.respond.failed",
            "2",
            json!({"requestId": "q", "detail": "Unknown pending user-input request q"}),
        );
        assert!(reconcile_activity(&thread, &mut list, &stale));
        assert_eq!(list.unwrap()[0].status, "confirmed");
    }
}
