//! Open approvals for a thread: a port of `derivePendingApprovals` in
//! `apps/web/src/pendingInteractionDerivation.ts` without the aggregate
//! fallback (the native client always has the thread detail).
//!
//! Requests replay over activities in provider order, keyed by
//! `requestId` + `lifecycleGeneration`; a resolution closes them; explicit
//! stale-callback failures close them; and when the server sent
//! `pendingInteractions`, only `pending`/`retryable` settlements stay open.

use std::collections::{HashMap, HashSet};

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
        "stale pending user-input request",
        "stale pending user input request",
        "unknown pending user-input request",
        "unknown pending user input request",
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

fn actionable(settlements: &[PendingInteraction], kind: &str) -> HashSet<(String, Option<String>)> {
    settlements
        .iter()
        .filter(|s| s.interaction_kind == kind)
        .filter(|s| matches!(s.status.as_str(), "pending" | "retryable"))
        .map(|s| (s.request_id.clone(), s.lifecycle_generation.clone()))
        .collect()
}

/// One open request instance found by [`replay`].
struct Open<T> {
    request_id: String,
    lifecycle_generation: Option<String>,
    created_at: String,
    value: T,
}

/// `replayPendingInteractions` for one interaction kind (`approval` or
/// `userInput`): `parse` turns a requested activity's payload into `T`, or
/// skips it.
fn replay<T>(
    thread: &Thread,
    kind: &str,
    requested: &str,
    resolved: &str,
    failed: &str,
    parse: impl Fn(&Value) -> Option<T>,
) -> Vec<Open<T>> {
    let activities = ordered(&thread.activities);
    let mut open: Vec<Open<T>> = Vec::new();
    for activity in &activities {
        let payload = &activity.payload;
        let Some(request_id) = text(payload, "requestId") else {
            continue;
        };
        let generation = text(payload, "lifecycleGeneration").map(str::to_owned);
        if activity.kind == requested {
            let Some(value) = parse(payload) else {
                continue;
            };
            // `replacePendingInteraction`: a new request replaces every
            // earlier instance of the same id.
            open.retain(|p| p.request_id != request_id);
            open.push(Open {
                request_id: request_id.to_owned(),
                lifecycle_generation: generation,
                created_at: activity.created_at.clone(),
                value,
            });
        } else if activity.kind == resolved {
            open.retain(|p| {
                p.request_id != request_id
                    || (generation.is_some() && p.lifecycle_generation != generation)
            });
        }
    }
    // `createStalePendingInteractionMatcher`, applied after replay.
    for activity in &activities {
        if activity.kind != failed || !is_stale_detail(text(&activity.payload, "detail")) {
            continue;
        }
        let Some(request_id) = text(&activity.payload, "requestId") else {
            continue;
        };
        let generation = text(&activity.payload, "lifecycleGeneration");
        open.retain(|p| {
            let stale = match (generation, p.lifecycle_generation.as_deref()) {
                (Some(g), Some(pg)) => g == pg,
                (None, _) => activity.created_at >= p.created_at,
                (Some(_), None) => false,
            };
            !(p.request_id == request_id && stale)
        });
    }
    if let Some(settlements) = &thread.pending_interactions {
        let keep = actionable(settlements, kind);
        open.retain(|p| keep.contains(&(p.request_id.clone(), p.lifecycle_generation.clone())));
    }
    open.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    open
}

pub fn pending_approvals(thread: &Thread) -> Vec<PendingApproval> {
    replay(
        thread,
        "approval",
        "approval.requested",
        "approval.resolved",
        "provider.approval.respond.failed",
        |payload| {
            Some((
                request_kind(payload)?,
                text(payload, "detail").map(str::to_owned),
                text(payload, "toolName").map(str::to_owned),
                payload
                    .get("sessionApprovalAvailable")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ))
        },
    )
    .into_iter()
    .map(|open| {
        let (request_kind, detail, tool_name, session_approval_available) = open.value;
        PendingApproval {
            request_id: open.request_id,
            lifecycle_generation: open.lifecycle_generation,
            request_kind,
            created_at: open.created_at,
            detail,
            tool_name,
            session_approval_available,
        }
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// `UserInputQuestion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUserInput {
    pub request_id: String,
    pub lifecycle_generation: Option<String>,
    pub created_at: String,
    pub questions: Vec<Question>,
}

/// `parseUserInputQuestions` in `apps/web/src/workLog.ts`.
pub fn parse_questions(payload: &Value) -> Option<Vec<Question>> {
    let raw = payload.get("questions")?.as_array()?;
    let field = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let questions: Vec<Question> = raw
        .iter()
        .filter_map(|q| {
            Some(Question {
                id: field(q, "id")?,
                header: field(q, "header")?,
                question: field(q, "question")?,
                options: q
                    .get("options")?
                    .as_array()?
                    .iter()
                    .filter_map(|o| {
                        Some(QuestionOption {
                            label: field(o, "label")?,
                            description: field(o, "description")?,
                        })
                    })
                    .collect(),
                multi_select: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect();
    (!questions.is_empty()).then_some(questions)
}

pub fn pending_user_inputs(thread: &Thread) -> Vec<PendingUserInput> {
    replay(
        thread,
        "userInput",
        "user-input.requested",
        "user-input.resolved",
        "provider.user-input.respond.failed",
        parse_questions,
    )
    .into_iter()
    .map(|open| PendingUserInput {
        request_id: open.request_id,
        lifecycle_generation: open.lifecycle_generation,
        created_at: open.created_at,
        questions: open.value,
    })
    .collect()
}

/// A draft answer to one question: picked option labels, or typed text,
/// which wins (`resolvePendingUserInputAnswer` in
/// `apps/web/src/pendingUserInput.ts`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftAnswer {
    pub selected: Vec<String>,
    pub custom: Option<String>,
}

impl DraftAnswer {
    /// Picks an option: toggles it for multi-select, replaces otherwise.
    pub fn pick(&mut self, question: &Question, label: &str) {
        self.custom = None;
        if question.multi_select {
            if let Some(at) = self.selected.iter().position(|l| l == label) {
                self.selected.remove(at);
            } else {
                self.selected.push(label.to_owned());
            }
        } else {
            self.selected = vec![label.to_owned()];
        }
    }

    pub fn resolve(&self, question: &Question) -> Option<Value> {
        if let Some(custom) = self
            .custom
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(Value::String(custom.to_owned()));
        }
        let selected: Vec<&String> = self
            .selected
            .iter()
            .filter(|l| !l.trim().is_empty())
            .collect();
        if question.multi_select {
            (!selected.is_empty()).then(|| {
                Value::Array(
                    selected
                        .into_iter()
                        .map(|l| Value::String(l.clone()))
                        .collect(),
                )
            })
        } else {
            selected.first().map(|l| Value::String((*l).clone()))
        }
    }
}

/// `buildPendingUserInputAnswers`: answers keyed by question id, or `None`
/// until every question has one.
pub fn build_answers(
    questions: &[Question],
    drafts: &HashMap<String, DraftAnswer>,
) -> Option<serde_json::Map<String, Value>> {
    questions
        .iter()
        .map(|q| Some((q.id.clone(), drafts.get(&q.id)?.resolve(q)?)))
        .collect()
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
    fn parses_questions_and_builds_answers() {
        let t = thread(
            json!([act(
                "a",
                "user-input.requested",
                "1",
                json!({"requestId": "q1", "questions": [
                    {"id": "lang", "header": "Language", "question": "Which?", "options": [
                        {"label": "Rust", "description": "fast"}, {"label": "Go", "description": "simple"}]},
                    {"id": "tags", "header": "Tags", "question": "Pick some", "multiSelect": true,
                     "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
                    {"id": "broken"}
                ]})
            )]),
            None,
        );
        let open = pending_user_inputs(&t);
        assert_eq!(open.len(), 1);
        let questions = &open[0].questions;
        assert_eq!(questions.len(), 2);
        assert!(questions[1].multi_select);

        let mut drafts: HashMap<String, DraftAnswer> = HashMap::new();
        drafts
            .entry("lang".into())
            .or_default()
            .pick(&questions[0], "Rust");
        drafts
            .entry("lang".into())
            .or_default()
            .pick(&questions[0], "Go");
        assert!(build_answers(questions, &drafts).is_none());
        let tags = drafts.entry("tags".into()).or_default();
        tags.pick(&questions[1], "a");
        tags.pick(&questions[1], "b");
        tags.pick(&questions[1], "a");
        let answers = build_answers(questions, &drafts).unwrap();
        assert_eq!(answers["lang"], json!("Go"));
        assert_eq!(answers["tags"], json!(["b"]));

        drafts.get_mut("lang").unwrap().custom = Some(" Zig ".into());
        assert_eq!(
            build_answers(questions, &drafts).unwrap()["lang"],
            json!("Zig")
        );
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
