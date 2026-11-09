//! A reduced port of `deriveWorkLogEntries` in `apps/web/src/workLog.ts`:
//! the activity rows a transcript shows between messages, plus one
//! "Task interrupted" row per failed turn (`deriveTurnFailureEntries`).
//!
//! Ported: the quiet-activity filters, tool lifecycle collapse by call id,
//! detail from `payload.detail`, and the failure rows. Not yet ported: the
//! specialised cards (subagents, background tasks, handoffs, visuals,
//! question/answer exchanges); those activities show as plain rows.

use std::collections::HashMap;

use serde_json::Value;
use synara_protocol::orchestration::Activity;

/// Longest detail shown under a work row.
const MAX_DETAIL_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Thinking,
    Tool,
    Info,
    Error,
}

impl Tone {
    fn from_wire(tone: &str) -> Self {
        match tone {
            "thinking" => Self::Thinking,
            "tool" => Self::Tool,
            "error" => Self::Error,
            // `approval` shows as info, as on the web.
            _ => Self::Info,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkEntry {
    /// `activity.id`, or `turn-failure:<turnId>` for failure rows.
    pub id: String,
    pub created_at: String,
    pub tone: Tone,
    pub label: String,
    pub detail: Option<String>,
    /// Set on failure rows: the sentence the web shows under the label.
    pub failure: Option<String>,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_turn_failure(activity: &Activity) -> bool {
    activity.turn_id.is_some()
        && (activity.kind == "runtime.error"
            || (activity.kind == "turn.completed"
                && (text(&activity.payload, "state") == Some("failed")
                    || activity.tone == "error")))
}

/// `isQuietTurnLifecycleActivity`, `isQuietApprovalResolutionActivity` and
/// the kind filters in `deriveWorkLogEntries`.
fn is_quiet(activity: &Activity) -> bool {
    let kind = activity.kind.as_str();
    match kind {
        "turn.started" | "turn.stop-requested" => true,
        "turn.completed" | "turn.aborted" => activity.tone != "error",
        "task.started" | "task.updated" | "task.completed" => true,
        "account.rate-limits.updated" | "context-window.updated" | "context-window.configured" => {
            true
        }
        "approval.resolved" => {
            activity.tone != "error"
                && text(&activity.payload, "decision") == Some("accept")
                && activity.payload.get("approvalScope").is_none()
        }
        _ => activity.summary == "Checkpoint captured",
    }
}

/// `extractToolCallId`.
fn tool_call_id(payload: &Value) -> Option<&str> {
    let data = payload.get("data")?;
    ["toolCallId", "toolUseId", "callID", "callId"]
        .iter()
        .find_map(|key| text(data, key))
        .or_else(|| data.get("item").and_then(|item| text(item, "id")))
}

/// Strips ANSI escape sequences (`stripTerminalControlSequences`, CSI only).
fn strip_ansi(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn cap(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let cut: String = value.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

fn detail(activity: &Activity) -> Option<String> {
    let raw = text(&activity.payload, "detail").or_else(|| {
        (activity.kind == "runtime.warning")
            .then(|| text(&activity.payload, "message"))
            .flatten()
    })?;
    let clean = strip_ansi(raw);
    let clean = clean.trim();
    (!clean.is_empty()).then(|| cap(clean, MAX_DETAIL_CHARS))
}

/// Activities in provider order (`compareActivitiesByOrder`): sequenced
/// rows by sequence after unsequenced ones, then by `createdAt`.
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

pub fn work_entries(activities: &[Activity]) -> Vec<WorkEntry> {
    let ordered = ordered(activities);
    let mut entries: Vec<WorkEntry> = Vec::new();
    // One row per tool call: later lifecycle events update it in place.
    let mut by_call: HashMap<&str, usize> = HashMap::new();
    for activity in &ordered {
        if is_turn_failure(activity) || is_quiet(activity) {
            continue;
        }
        let entry = WorkEntry {
            id: activity.id.clone(),
            created_at: activity.created_at.clone(),
            tone: Tone::from_wire(&activity.tone),
            label: activity.summary.clone(),
            detail: detail(activity),
            failure: None,
        };
        match tool_call_id(&activity.payload) {
            Some(call) => match by_call.get(call) {
                Some(&at) => {
                    let previous = &entries[at];
                    entries[at] = WorkEntry {
                        id: previous.id.clone(),
                        created_at: previous.created_at.clone(),
                        detail: entry.detail.or_else(|| previous.detail.clone()),
                        ..entry
                    };
                }
                None => {
                    by_call.insert(call, entries.len());
                    entries.push(entry);
                }
            },
            None => entries.push(entry),
        }
    }
    entries.extend(failure_entries(&ordered));
    entries
}

/// `deriveTurnFailureEntries`: one row per failed turn that did not later
/// complete, cancel or get interrupted.
fn failure_entries(ordered: &[&Activity]) -> Vec<WorkEntry> {
    let mut terminal: HashMap<&str, &str> = HashMap::new();
    for activity in ordered {
        let Some(turn) = activity.turn_id.as_ref() else {
            continue;
        };
        match activity.kind.as_str() {
            "turn.aborted" => {
                terminal.insert(turn.as_str(), "interrupted");
            }
            "turn.completed" => {
                terminal.insert(
                    turn.as_str(),
                    text(&activity.payload, "state").unwrap_or(""),
                );
            }
            _ => {}
        }
    }
    let mut failures: Vec<WorkEntry> = Vec::new();
    let mut causes: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for activity in ordered {
        if !is_turn_failure(activity) {
            continue;
        }
        let turn = activity.turn_id.as_ref().map(|t| t.as_str()).unwrap_or("");
        if matches!(
            terminal.get(turn),
            Some(&("completed" | "cancelled" | "interrupted"))
        ) {
            continue;
        }
        let id = format!("turn-failure:{turn}");
        let payload = &activity.payload;
        let (cause, code) = causes.entry(id.clone()).or_default();
        if let Some(c) = text(payload, "errorMessage").or_else(|| text(payload, "message")) {
            *cause = Some(c.to_owned());
        }
        if let Some(c) = text(payload, "errorCode") {
            *code = Some(c.to_owned());
        }
        let cause_text = cause
            .clone()
            .unwrap_or_else(|| "The provider reported an error.".to_owned());
        let overloaded = code.as_deref() == Some("server_overloaded")
            || cause_text
                .to_lowercase()
                .contains("selected model is at capacity");
        let message = if overloaded {
            "The task was interrupted because the model is at capacity. Work remains incomplete."
                .to_owned()
        } else {
            format!(
                "The task was interrupted by a provider error. Work remains incomplete. {}",
                cap(strip_ansi(&cause_text).trim(), MAX_DETAIL_CHARS)
            )
        };
        let row = WorkEntry {
            id: id.clone(),
            created_at: activity.created_at.clone(),
            tone: Tone::Error,
            label: "Task interrupted".to_owned(),
            detail: None,
            failure: Some(message),
        };
        match failures.iter_mut().find(|f| f.id == id) {
            Some(existing) => {
                existing.failure = row.failure;
            }
            None => failures.push(row),
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(id: &str, kind: &str, tone: &str, turn: Option<&str>, payload: Value) -> Activity {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": kind, "tone": tone, "summary": format!("{kind} {id}"),
            "payload": payload, "turnId": turn, "createdAt": format!("2026-01-01T00:00:0{}Z", id.len()),
        }))
        .unwrap()
    }

    #[test]
    fn filters_quiet_rows_and_collapses_tool_calls() {
        let rows = work_entries(&[
            activity("a", "turn.started", "info", Some("t"), Value::Null),
            activity(
                "b",
                "tool.started",
                "tool",
                Some("t"),
                serde_json::json!({"data": {"toolCallId": "c1"}, "detail": "ls"}),
            ),
            activity(
                "bb",
                "tool.completed",
                "tool",
                Some("t"),
                serde_json::json!({"data": {"toolCallId": "c1"}, "detail": "\u{1b}[31mok\u{1b}[0m"}),
            ),
            activity(
                "ccc",
                "turn.completed",
                "info",
                Some("t"),
                serde_json::json!({"state": "completed"}),
            ),
            activity(
                "dddd",
                "approval.resolved",
                "approval",
                Some("t"),
                serde_json::json!({"decision": "accept"}),
            ),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "b");
        assert_eq!(rows[0].label, "tool.completed bb");
        assert_eq!(rows[0].detail.as_deref(), Some("ok"));
        assert_eq!(rows[0].tone, Tone::Tool);
    }

    #[test]
    fn one_failure_row_per_failed_turn() {
        let error = serde_json::json!({"class": "provider_error",
            "message": "unexpected status 401 Unauthorized"});
        let rows = work_entries(&[
            activity("a", "runtime.error", "error", Some("t1"), error.clone()),
            activity(
                "bb",
                "turn.completed",
                "error",
                Some("t1"),
                serde_json::json!({"state": "failed"}),
            ),
            activity("ccc", "runtime.error", "error", Some("t2"), error),
            activity("dddd", "turn.aborted", "info", Some("t2"), Value::Null),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "turn-failure:t1");
        assert_eq!(rows[0].label, "Task interrupted");
        assert!(rows[0]
            .failure
            .as_deref()
            .unwrap()
            .ends_with("Work remains incomplete. unexpected status 401 Unauthorized"));
    }

    #[test]
    fn capacity_failures_get_the_capacity_sentence() {
        let rows = work_entries(&[activity(
            "a",
            "runtime.error",
            "error",
            Some("t"),
            serde_json::json!({"errorCode": "server_overloaded"}),
        )]);
        assert!(rows[0].failure.as_deref().unwrap().contains("at capacity"));
    }
}
